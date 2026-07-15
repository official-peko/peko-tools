use super::*;
use crate::PekoError;

#[test]
fn operating_system_from_name_known_values() {
    assert_eq!(OperatingSystem::from_name("macos"), OperatingSystem::MacOS);
    assert_eq!(
        OperatingSystem::from_name("windows"),
        OperatingSystem::Windows
    );
    assert_eq!(OperatingSystem::from_name("linux"), OperatingSystem::Linux);
    assert_eq!(
        OperatingSystem::from_name("android"),
        OperatingSystem::Android
    );
    assert_eq!(OperatingSystem::from_name("ios"), OperatingSystem::IOS);
}

#[test]
fn operating_system_from_name_unknown_is_unknown() {
    assert_eq!(
        OperatingSystem::from_name("plan9"),
        OperatingSystem::Unknown
    );
    assert_eq!(OperatingSystem::from_name(""), OperatingSystem::Unknown);
    assert_eq!(
        OperatingSystem::from_name("LINUX"),
        OperatingSystem::Unknown
    ); // case-sensitive
}

#[test]
fn operating_system_display_roundtrips_with_from_name() {
    for os in [
        OperatingSystem::MacOS,
        OperatingSystem::Windows,
        OperatingSystem::Linux,
        OperatingSystem::Android,
        OperatingSystem::IOS,
        OperatingSystem::Unknown,
    ] {
        let rendered = format!("{os}");
        assert_eq!(OperatingSystem::from_name(&rendered), os);
    }
}

#[test]
fn architecture_from_name_known_values() {
    assert_eq!(Architecture::from_name("arm"), Architecture::Arm);
    assert_eq!(Architecture::from_name("x86_64"), Architecture::X86_64);
}

#[test]
fn architecture_from_name_unknown_is_unknown() {
    assert_eq!(Architecture::from_name("riscv"), Architecture::Unknown);
    assert_eq!(Architecture::from_name(""), Architecture::Unknown);
}

#[test]
fn architecture_display_roundtrips_with_from_name() {
    for arch in [
        Architecture::Arm,
        Architecture::X86_64,
        Architecture::Unknown,
    ] {
        let rendered = format!("{arch}");
        assert_eq!(Architecture::from_name(&rendered), arch);
    }
}

#[test]
fn from_descriptor_two_part_form_disables_console() {
    let t = PekoTarget::from_descriptor("linux-x86_64").unwrap();
    assert_eq!(t.operating_system, OperatingSystem::Linux);
    assert_eq!(t.architecture, Architecture::X86_64);
    assert!(!t.console);
}

#[test]
fn from_descriptor_three_part_form_enables_console() {
    let t = PekoTarget::from_descriptor("windows-x86_64-console").unwrap();
    assert_eq!(t.operating_system, OperatingSystem::Windows);
    assert_eq!(t.architecture, Architecture::X86_64);
    assert!(t.console);
}

#[test]
fn from_descriptor_unknown_names_are_not_errors() {
    let t = PekoTarget::from_descriptor("plan9-riscv").unwrap();
    assert_eq!(t.operating_system, OperatingSystem::Unknown);
    assert_eq!(t.architecture, Architecture::Unknown);
}

#[test]
fn from_descriptor_rejects_single_token() {
    let err = PekoTarget::from_descriptor("linux").expect_err("single-token must fail");
    match err {
        PekoError::InvalidTargetDescriptor(s) => assert_eq!(s, "linux"),
        other => panic!("expected InvalidTargetDescriptor, got {other:?}"),
    }
}

#[test]
fn from_descriptor_rejects_empty_string() {
    let err = PekoTarget::from_descriptor("").expect_err("empty must fail");
    assert!(matches!(err, PekoError::InvalidTargetDescriptor(_)));
}

#[test]
fn fromstr_delegates_to_from_descriptor() {
    let parsed: PekoTarget = "macos-arm".parse().unwrap();
    assert_eq!(parsed.operating_system, OperatingSystem::MacOS);
    assert_eq!(parsed.architecture, Architecture::Arm);
}

#[test]
fn display_roundtrips_with_from_descriptor() {
    let t = PekoTarget::new(OperatingSystem::Linux, Architecture::X86_64, false);
    let rendered = format!("{t}");
    let parsed = PekoTarget::from_descriptor(&rendered).unwrap();
    assert_eq!(parsed, t);

    let t = PekoTarget::new(OperatingSystem::Windows, Architecture::X86_64, true);
    let rendered = format!("{t}");
    let parsed = PekoTarget::from_descriptor(&rendered).unwrap();
    assert_eq!(parsed, t);
}

#[test]
fn to_triple_android_by_arch() {
    // arm64 for devices, x86_64 for emulators.
    let t = PekoTarget::new(OperatingSystem::Android, Architecture::Arm, false);
    assert_eq!(t.to_triple(), "aarch64-unknown-linux-android19");
    let t = PekoTarget::new(OperatingSystem::Android, Architecture::X86_64, false);
    assert_eq!(t.to_triple(), "x86_64-unknown-linux-android19");
}

#[test]
fn to_triple_linux_arm() {
    let t = PekoTarget::new(OperatingSystem::Linux, Architecture::Arm, false);
    assert_eq!(t.to_triple(), "arm64-pc-linux-gnu");
}

#[test]
fn to_triple_macos_x86_64() {
    let t = PekoTarget::new(OperatingSystem::MacOS, Architecture::X86_64, false);
    assert_eq!(t.to_triple(), "x86_64-apple-darwin20.6.0");
}

#[test]
fn to_triple_windows_x86_64() {
    let t = PekoTarget::new(OperatingSystem::Windows, Architecture::X86_64, true);
    assert_eq!(t.to_triple(), "x86_64-pc-win32");
}

#[test]
fn to_triple_ios_arm() {
    let t = PekoTarget::new(OperatingSystem::IOS, Architecture::Arm, false);
    assert_eq!(t.to_triple(), "arm64-apple-ios16.4.0");
}

#[test]
fn to_triple_unknown_os_yields_arch_prefix_only() {
    let t = PekoTarget::new(OperatingSystem::Unknown, Architecture::X86_64, false);
    assert_eq!(t.to_triple(), "x86_64-");
}

#[test]
fn default_does_not_panic_and_console_is_true() {
    // The previous implementation panicked on unsupported hosts; this test
    // exists to lock in that the new implementation never panics.
    let t = PekoTarget::default();
    assert!(t.console);
    // We don't assert the OS/arch values since they depend on the test host,
    // but constructing the value must succeed.
    let _ = format!("{t}");
}
