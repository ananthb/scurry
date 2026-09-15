//! Firmware updates: finding a release, and pushing an image to the dongle.
//!
//! Split out of the binary because the tray needs every part of it too, and a
//! second implementation of "which release is newer" is a second thing to get
//! wrong.
//!
//! The transfer itself is deliberately dull. Each chunk is acknowledged before
//! the next goes out, which halves the theoretical throughput and is what makes
//! it safe: the dongle writes to flash as the bytes arrive, and a controller
//! that does not wait will outrun it and overflow a reassembly buffer that has
//! no allocator to grow. Over the cable the whole image takes a few seconds
//! anyway.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use scurry_proto::{ack, kind, FirmwareInfo, OtaBegin, OtaStatus, OTA_CHUNK_MAX};
use sha2::{Digest, Sha256};

/// Where releases come from. The repository is baked in rather than
/// configurable: an update source that can be pointed elsewhere by a config
/// file is a way to install firmware from anywhere, which is exactly the thing
/// the dongle's authorisation rules exist to prevent.
const RELEASES_API: &str = "https://api.github.com/repos/ananthb/scurry/releases/latest";

/// The asset a dongle takes. Only the app image is published: an update writes
/// the inactive app slot and nothing else.
pub const FIRMWARE_ASSET: &str = "scurry-dongle-esp32c3.bin";

/// GitHub rejects requests without one.
const USER_AGENT: &str = concat!("scurry-ctl/", env!("CARGO_PKG_VERSION"));

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// What the update needs from a link to the dongle.
///
/// A trait rather than a concrete type because the same transfer runs over the
/// cable, over BLE, and through the running app's control socket, and none of
/// those three know about each other.
pub trait Link {
    /// Send a request and return the payload of the reply, which must be of
    /// kind `want`.
    fn request(&mut self, kind: u8, payload: &[u8], want: u8) -> Result<Vec<u8>>;
}

/// A release, reduced to the parts that matter here.
#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub firmware_url: String,
    /// The merged image that provisions a blank board. Absent on releases
    /// that predate it.
    pub factory_url: Option<String>,
    /// URL of the signed checksum manifest, when the release has one.
    pub checksums_url: Option<String>,
}

/// Ask GitHub what the latest release is.
pub fn latest_release() -> Result<Release> {
    let body: serde_json::Value = ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .get(RELEASES_API)
        .call()
        .context("asking GitHub for the latest release")?
        .into_json()
        .context("the release listing was not JSON")?;

    let tag = body["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow!("the release has no tag"))?
        .to_string();

    let assets = body["assets"]
        .as_array()
        .ok_or_else(|| anyhow!("the release lists no assets"))?;

    let find = |name: &str| -> Option<String> {
        assets.iter().find_map(|a| {
            (a["name"].as_str()? == name).then(|| a["browser_download_url"].as_str())?
                .map(str::to_string)
        })
    };

    let firmware_url = find(FIRMWARE_ASSET).ok_or_else(|| {
        anyhow!("release {tag} has no {FIRMWARE_ASSET}; it predates firmware updates")
    })?;

    Ok(Release {
        tag,
        firmware_url,
        factory_url: find(crate::provision::FACTORY_ASSET),
        checksums_url: find("SHA256SUMS"),
    })
}

fn get(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .get(url)
        .call()
        .with_context(|| format!("fetching {url}"))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .with_context(|| format!("reading {url}"))?;
    Ok(buf)
}

/// Download a release's firmware, checking it against the release's own
/// checksum manifest.
///
/// Worth being precise about what that check is worth: it catches a truncated
/// or corrupted download, which is the failure that actually happens. It does
/// not establish that the release is genuine, because anyone who could alter
/// the image could alter the manifest beside it. The release also carries
/// `SHA256SUMS.sig` and a cosign certificate, and verifying those is what would
/// make this authentic rather than merely intact. That is a worthwhile next
/// step and is not done here.
pub fn download_firmware(release: &Release) -> Result<Vec<u8>> {
    download_asset(release, FIRMWARE_ASSET, &release.firmware_url)
}

/// Download the merged image that provisions a blank board, checked the same
/// way.
pub fn download_factory(release: &Release) -> Result<Vec<u8>> {
    let url = release.factory_url.as_ref().ok_or_else(|| {
        anyhow!(
            "release {} has no {}; it predates provisioning from the app",
            release.tag,
            crate::provision::FACTORY_ASSET
        )
    })?;
    download_asset(release, crate::provision::FACTORY_ASSET, url)
}

fn download_asset(release: &Release, name: &str, url: &str) -> Result<Vec<u8>> {
    let image = get(url)?;

    let Some(sums) = &release.checksums_url else {
        eprintln!("warning: release {} has no SHA256SUMS to check against", release.tag);
        return Ok(image);
    };

    let manifest = get(sums)?;
    let manifest = String::from_utf8_lossy(&manifest);
    let want = manifest
        .lines()
        .find_map(|l| {
            let (sum, got) = l.split_once("  ")?;
            (got.trim() == name).then(|| sum.trim().to_string())
        })
        .ok_or_else(|| anyhow!("SHA256SUMS does not mention {name}"))?;

    let got = hex(&Sha256::digest(&image));
    if got != want {
        bail!("the downloaded {name} does not match the release checksum:\n  expected {want}\n  got      {got}");
    }
    Ok(image)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compare two version strings the way a person would.
///
/// Dotted numbers, compared numerically, so 0.10.0 sorts after 0.9.0 where a
/// string comparison would put it before. A leading `v` is ignored because the
/// tag has one and the firmware's own version string may not. Anything that
/// does not parse compares as equal, which makes an unrecognised version
/// report "different" rather than confidently wrong.
fn version_parts(v: &str) -> Vec<u64> {
    v.trim_start_matches('v')
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// True when `candidate` is a strictly later version than `running`.
pub fn is_newer(candidate: &str, running: &str) -> bool {
    let (a, b) = (version_parts(candidate), version_parts(running));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Ask the dongle what it is running.
pub fn firmware_info(link: &mut dyn Link) -> Result<FirmwareInfo> {
    let payload = link
        .request(kind::GET_FIRMWARE, &[], kind::FIRMWARE)
        .context("asking the dongle what firmware it runs")?;
    FirmwareInfo::decode(&payload).ok_or_else(|| anyhow!("the firmware reply was too short"))
}

/// Push an image and reboot into it.
///
/// `progress` is called with (bytes sent, total) as the transfer proceeds.
pub fn flash(
    link: &mut dyn Link,
    image: &[u8],
    progress: &mut dyn FnMut(u32, u32),
) -> Result<()> {
    if image.is_empty() {
        bail!("the firmware image is empty");
    }
    // ESP-IDF app images start with 0xE9. Catching this here turns "you handed
    // me the bootloader, or a zip, or an HTML error page that a proxy served
    // instead of the file" into a sentence, rather than a flash erase followed
    // by a digest mismatch a minute later.
    if image[0] != 0xE9 {
        bail!(
            "that does not look like an ESP32 app image (first byte {:#04x}, expected 0xe9)",
            image[0]
        );
    }

    let total = image.len() as u32;
    let digest = Sha256::digest(image);

    let begin = OtaBegin {
        len: total,
        sha256: digest.into(),
    };
    let mut buf = [0u8; OtaBegin::WIRE_LEN];
    begin.encode_into(&mut buf);

    let ack = link
        .request(kind::OTA_BEGIN, &buf, kind::ACK)
        .context("starting the update")?;
    check_ack(&ack, "the dongle refused the image")?;

    let mut sent = 0usize;
    let mut chunk = vec![0u8; 4 + OTA_CHUNK_MAX];
    while sent < image.len() {
        let n = OTA_CHUNK_MAX.min(image.len() - sent);
        let used = scurry_proto::OtaChunk::encode_into(
            sent as u32,
            &image[sent..sent + n],
            &mut chunk,
        );

        // The reply is the flow control: the next chunk does not go out until
        // this one is on flash.
        let reply = link
            .request(kind::OTA_DATA, &chunk[..used], kind::OTA_STATUS)
            .map_err(|e| abort_and(link, e))
            .with_context(|| format!("sending bytes {sent}..{}", sent + n))?;

        let status = OtaStatus::decode(&reply)
            .ok_or_else(|| anyhow!("the dongle's progress reply was too short"))?;

        sent += n;
        if status.received != sent as u32 {
            abort(link);
            bail!(
                "the dongle has {} bytes but {sent} were sent; the transfer is out of step",
                status.received
            );
        }
        progress(sent as u32, total);
    }

    let ack = link
        .request(kind::OTA_END, &[], kind::ACK)
        .context("finishing the update")?;
    check_ack(&ack, "the dongle rejected the completed image")?;
    Ok(())
}

/// Tell the dongle to forget a transfer we are abandoning, so the next attempt
/// is not refused for arriving in the middle of this one. Best effort: the
/// error being reported is the one worth keeping.
fn abort(link: &mut dyn Link) {
    let _ = link.request(kind::OTA_ABORT, &[], kind::ACK);
}

fn abort_and(link: &mut dyn Link, e: anyhow::Error) -> anyhow::Error {
    abort(link);
    e
}

fn check_ack(payload: &[u8], what: &str) -> Result<()> {
    match payload.first().copied() {
        Some(ack::OK) => Ok(()),
        Some(ack::NOT_PERMITTED) => bail!(
            "{what}: not authorised. Firmware may be written over the cable, or by a \
             controller that has already been authorised -- press the dongle's button \
             three times and pair, or use the cable."
        ),
        Some(ack::OTA_FAILED) => bail!("{what}: the update failed. The dongle is still running the old firmware."),
        Some(ack::BAD_REQUEST) => bail!(
            "{what}: the dongle did not understand the request. Firmware older than \
             this tool cannot be updated over the wire -- use the cable and idf.py once, \
             and afterwards this will work."
        ),
        Some(other) => bail!("{what}: the dongle answered with code {other}"),
        None => bail!("{what}: the dongle sent an empty acknowledgement"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(is_newer("v0.10.0", "v0.9.0"));
        assert!(!is_newer("v0.9.0", "v0.10.0"));
        assert!(is_newer("0.2.0", "v0.1.9"));
        assert!(!is_newer("v0.1.0", "v0.1.0"));
    }

    #[test]
    fn a_longer_version_is_newer_only_if_the_extra_part_is_nonzero() {
        assert!(is_newer("v1.0.1", "v1.0"));
        assert!(!is_newer("v1.0.0", "v1.0"));
    }

    /// An unparseable version must not be reported as an upgrade. A dongle
    /// built from an untagged tree reports a bare commit hash, and offering to
    /// "update" it every time the tray polls would be a permanent nag.
    #[test]
    fn unparseable_versions_are_never_newer() {
        assert!(!is_newer("v0.2.0", "g1a2b3c4"));
        assert!(!is_newer("dirty", "v0.1.0"));
    }
}
