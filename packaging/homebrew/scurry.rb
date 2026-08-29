cask "scurry" do
  version :latest

  on_arm do
    url "https://github.com/ananthb/scurry/releases/latest/download/scurry-macos-arm64.dmg"
  end
  on_intel do
    url "https://github.com/ananthb/scurry/releases/latest/download/scurry-macos-amd64.dmg"
  end

  name "scurry"
  desc "Share one mouse and keyboard across machines"
  homepage "https://github.com/ananthb/scurry"

  app "scurry.app"
  # The CLI lives inside the bundle so there is one thing to install and one
  # thing to remove.
  binary "#{appdir}/scurry.app/Contents/MacOS/scurry-ctl"

  # Deliberately no postflight. The app registers its own login item from the
  # menu and asks for Accessibility through the system dialog, so there is
  # nothing to seed and nothing for the cask to explain.

  uninstall quit:       "com.ananthb.scurry-tray",
            launchctl:  "com.ananthb.scurry"

  # The layout lives on the dongle, not on this machine, so there is no config
  # to delete -- only the login item the app may have written.
  zap trash: "~/Library/LaunchAgents/com.ananthb.scurry.plist"

  caveats <<~EOS
    scurry needs Accessibility permission to capture the pointer. It will ask
    the first time you open it.
  EOS
end
