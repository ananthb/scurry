idf_component_register(SRCS "ble_hidd_demo_main.c"
                            "esp_hidd_prf_api.c"
                            "hid_dev.c"
                            "hid_device_le_prf.c"
                            "scurry_ctl_svc.c"
                    PRIV_REQUIRES bt nvs_flash esp_driver_gpio esp_driver_usb_serial_jtag esp_ringbuf
                    INCLUDE_DIRS ".")

target_compile_options(${COMPONENT_LIB} PRIVATE -Wno-unused-const-variable)

# The layout engine is Rust, compiled no_std for this target and linked in as a
# static library. Building it here rather than as a checked-in artefact means it
# cannot silently go stale against the crate sources.
set(SCURRY_FFI_DIR ${CMAKE_CURRENT_LIST_DIR}/../../../crates/scurry-ffi)
set(SCURRY_FFI_LIB ${SCURRY_FFI_DIR}/target/riscv32imc-unknown-none-elf/release/libscurry_ffi.a)

# An unconditional target rather than add_custom_command(OUTPUT ...): that form
# only reruns when its declared dependencies change, and listing every Rust
# source here would go stale silently -- linking a months-old .a while the crate
# had moved on. Cargo already knows what needs rebuilding and no-ops otherwise.
add_custom_target(scurry_ffi_build ALL
    COMMAND cargo build --release --target riscv32imc-unknown-none-elf
    WORKING_DIRECTORY ${SCURRY_FFI_DIR}
    BYPRODUCTS ${SCURRY_FFI_LIB}
    COMMENT "Building scurry-ffi (Rust layout engine) for riscv32imc"
    VERBATIM)
add_dependencies(${COMPONENT_LIB} scurry_ffi_build)
target_link_libraries(${COMPONENT_LIB} PRIVATE ${SCURRY_FFI_LIB})
