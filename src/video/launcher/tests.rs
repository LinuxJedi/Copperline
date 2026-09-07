use super::*;

#[test]
fn run_ahead_frames_survives_the_config_screen_round_trip() {
    let raw: RawConfig = toml::from_str("[emulation]\nrun_ahead_frames = 2\n").unwrap();
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.to_raw().emulation.run_ahead_frames, Some(2));
}

#[test]
fn warp_boot_rows_cycle_and_round_trip() {
    let raw: RawConfig = toml::from_str("").unwrap();
    let mut setup = MachineSetup::from_raw(&raw).unwrap();
    // Default: off, nothing emitted.
    assert_eq!(setup.to_raw().emulation.warp_boot, None);
    setup.cycle(F::WarpBoot, true);
    assert_eq!(setup.to_raw().emulation.warp_boot, Some(true));
    setup.cycle(F::WarpBootIdle, true); // 10s -> 15s
    assert_eq!(setup.to_raw().emulation.warp_boot_idle, Some(15.0));
    setup.cycle(F::WarpBoot, true); // back to off
    assert_eq!(setup.to_raw().emulation.warp_boot, None);

    // A TOML warp_until shows as its own state; one press clears it (the
    // modes are mutually exclusive) and lands on Off.
    let raw: RawConfig = toml::from_str("[emulation]\nwarp_until = 12.5\n").unwrap();
    let mut setup = MachineSetup::from_raw(&raw).unwrap();
    // A fractional configured timestamp is shown as configured, not
    // rounded into a different-looking one.
    assert_eq!(setup.value_label(F::WarpBoot), "Until 12.5s");
    setup.cycle(F::WarpBoot, true);
    assert_eq!(setup.value_label(F::WarpBoot), "Disabled");
    assert_eq!(setup.value_label(F::WarpBootIdle), "10s");
    let out = setup.to_raw();
    assert_eq!(out.emulation.warp_until, None);
    assert_eq!(out.emulation.warp_boot, None);
}

#[test]
fn warp_boot_settings_survive_the_config_screen_round_trip() {
    // No config-screen controls yet, same as run-ahead: a saved default
    // opening in the launcher must not silently shed them on Run/Save.
    let raw: RawConfig =
        toml::from_str("[emulation]\nwarp_boot = true\nwarp_boot_idle = 15.0\n").unwrap();
    let setup = MachineSetup::from_raw(&raw).unwrap();
    let out = setup.to_raw();
    assert_eq!(out.emulation.warp_boot, Some(true));
    assert_eq!(out.emulation.warp_boot_idle, Some(15.0));
    assert_eq!(out.emulation.warp_until, None);

    let raw: RawConfig = toml::from_str("[emulation]\nwarp_until = 12.5\n").unwrap();
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.to_raw().emulation.warp_until, Some(12.5));
}

#[test]
fn uaelib_setting_survives_the_config_screen_round_trip() {
    // No config-screen control: a saved `uaelib = false` must not revert
    // to the default on Run/Save.
    let raw: RawConfig =
        toml::from_str("[emulation]\nuaelib = false\nuaelib_files = true\n").unwrap();
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.to_raw().emulation.uaelib, Some(false));
    assert_eq!(setup.to_raw().emulation.uaelib_files, Some(true));
    let raw: RawConfig = toml::from_str("").unwrap();
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.to_raw().emulation.uaelib, None);
    assert_eq!(setup.to_raw().emulation.uaelib_files, None);
}

#[test]
fn ram_initialisation_controls_cycle_and_round_trip() {
    let raw: RawConfig = toml::from_str("[memory]\ninit = \"random:0xBEEF\"\n").unwrap();
    let mut setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.value_label(F::RamInit), "Random");
    assert_eq!(
        setup.to_raw().memory.init.as_deref(),
        Some("random:0x000000000000BEEF")
    );

    setup.cycle(F::RamInit, true);
    assert_eq!(setup.value_label(F::RamInit), "Fixed");
    assert_eq!(setup.value_label(F::RamPattern), "0x5555");
    assert!(setup.applies(F::RamPattern));
    setup.set_ram_pattern(0xDEAD);
    assert_eq!(
        setup.to_raw().memory.init.as_deref(),
        Some("pattern:0xDEAD")
    );

    setup.cycle(F::RamInit, true);
    assert_eq!(setup.value_label(F::RamInit), "Zero");
    // The pattern row disappears rather than greying: it only
    // exists while the fill is Fixed.
    assert!(setup.row_hidden(F::RamPattern));
    setup.cycle(F::RamInit, false);
    assert!(!setup.row_hidden(F::RamPattern));
    assert_eq!(setup.value_label(F::RamPattern), "0xDEAD");

    setup.cycle(F::RamInit, false);
    assert_eq!(setup.value_label(F::RamInit), "Random");
    assert_eq!(
        setup.to_raw().memory.init.as_deref(),
        Some("random:0x000000000000BEEF")
    );
}

#[test]
fn fixed_ram_pattern_text_box_validates_and_commits() {
    let mut setup = MachineSetup::default();
    setup.cycle(F::RamInit, false); // Zero -> Fixed
    let mut state = LauncherState::new(setup);
    state.begin_edit_ram_pattern();
    assert_eq!(state.editing(), Some(EditTarget::RamPattern));
    assert_eq!(state.edit_buffer(), "0x5555");

    state.edit_buffer.clear();
    for c in "0x1234".chars() {
        state.edit_push(c);
    }
    state.edit_commit();
    assert_eq!(state.setup.value_label(F::RamPattern), "0x1234");
    assert_eq!(
        state.setup.to_raw().memory.init.as_deref(),
        Some("pattern:0x1234")
    );

    state.begin_edit_ram_pattern();
    state.edit_buffer = "0x10000".to_string();
    state.edit_caret = Caret::end_of(&state.edit_buffer);
    state.edit_commit();
    assert_eq!(state.editing(), Some(EditTarget::RamPattern));
    assert!(state
        .status
        .as_ref()
        .is_some_and(|s| s.kind == StatusKind::Error));
    assert_eq!(state.setup.value_label(F::RamPattern), "0x1234");
}

/// The page greys what an interface does not honour, exactly as the
/// library's own driver table reports it: the launcher carries no driver
/// knowledge of its own, so the expectation here is derived from the same
/// table and covers whichever drivers this build compiled in. A driver
/// the build does not carry deliberately greys nothing -- there is
/// nothing to ask, and a dead page cannot be fixed from the page.
#[cfg(feature = "fluxbridge")]
#[test]
fn bridge_rows_grey_what_the_interface_does_not_support() {
    let mut setup = MachineSetup::default();
    setup.set_drive_bridged(0, true);
    setup.set_bridge_edit_drive(0);
    // Capability greying is only reachable with an interface attached and
    // selected, and with a port to pick; without either the page greys
    // first (see `bridge_page_greys_without_an_interface`). Pinned rather
    // than sampled: what a test machine has attached is not this test's
    // subject.
    setup.bridge_status = BridgeStatus::Attached;
    setup.df_bridge_none[0] = false;
    setup.bridge_ports = vec![None, Some("/dev/ttyACM0".to_string())];

    let offered = bridge_drivers();
    assert!(!offered.is_empty(), "the bridge offers its drivers");
    for driver in offered {
        let info = crate::fluxbridge::driver_named(driver.match_token())
            .expect("offered drivers come from the library's table");
        let has_select = info.supports(crate::fluxbridge::config_option::DRIVE_AB_CABLE)
            || info.supports(crate::fluxbridge::config_option::SUPPORTS_SHUGART);
        setup.df_bridge[0].as_mut().expect("bridged").driver = driver;
        assert_eq!(
            setup.disabled_reason(F::BridgeCable).is_none(),
            has_select,
            "drive select on {driver:?}"
        );
        // Every interface here talks over a serial port.
        assert!(setup.disabled_reason(F::BridgePort).is_none());
    }
}

/// A disk chosen on the Host Disk page reaches the machine's
/// configuration, and comes back the same when it is read again -- which
/// is what makes an attachment outlive the session that made it.
#[test]
fn a_mounted_host_disk_round_trips_through_the_configuration() {
    let mut setup = MachineSetup::default();
    setup.set_host_disks_for_test(vec![
        HostDiskRow {
            id: "disk4".to_string(),
            fingerprint: None,
            volume: "SanDisk".to_string(),
            size: "31.9 GB".to_string(),
            mounted: Vec::new(),
            writable: true,
            attach: Some(crate::config::HostDiskAttach::IdeSlave),
        },
        HostDiskRow {
            id: "disk9".to_string(),
            fingerprint: None,
            volume: "Kingston".to_string(),
            size: "3.9 GB".to_string(),
            mounted: Vec::new(),
            writable: false,
            attach: Some(crate::config::HostDiskAttach::IdeMaster),
        },
    ]);

    setup.select_model(Some(MachineModel::A1200));
    // Ticking claims the first free attachment point, whatever the row
    // happened to be showing.
    setup.select_host_disk(0);
    assert_eq!(
        setup.host_disks()[0].attach,
        Some(crate::config::HostDiskAttach::IdeMaster)
    );
    // The second disk lands beside the first, not on it.
    setup.select_host_disk(1);
    assert_eq!(
        setup.host_disks()[1].attach,
        Some(crate::config::HostDiskAttach::IdeSlave)
    );

    let mounted = setup.mount_host_disks().expect("ticked disks mount");
    assert_eq!(mounted.len(), 2);
    assert_eq!(mounted[0].device, "disk4");
    assert_eq!(mounted[0].attach, crate::config::HostDiskAttach::IdeMaster);
    assert!(mounted[0].writable, "the choice to allow writing carries");
    assert!(!mounted[1].writable);
    // Mounting leaves the ticks alone: they show what the machine has, so
    // coming back to the page shows the machine rather than a blank slate.
    assert!(setup.host_disk_is_selected("disk4"));

    // Each shows on the row that holds it.
    assert_eq!(
        setup
            .host_disk_on_row(LauncherField::IdeMaster)
            .map(|d| d.device.as_str()),
        Some("disk4")
    );
    assert_eq!(
        setup
            .host_disk_on_row(LauncherField::IdeSlave)
            .map(|d| d.device.as_str()),
        Some("disk9")
    );

    // Out to a configuration file, and back.
    let raw = setup.to_raw();
    assert_eq!(raw.host_disk.len(), 2);
    assert_eq!(raw.host_disk[0].device, "disk4");
    assert_eq!(raw.host_disk[0].attach.as_deref(), Some("ide-master"));
    // Physical-disk access is always explicit in a saved configuration;
    // older entries with no field safely load read-only.
    assert_eq!(raw.host_disk[0].read_only, Some(false));
    assert_eq!(raw.host_disk[1].read_only, Some(true));

    let reloaded = MachineSetup::from_raw(&raw).expect("the written configuration reads back");
    let back = reloaded
        .host_disk_at(crate::config::HostDiskAttach::IdeMaster)
        .expect("the attachment survives being written and read");
    assert_eq!(back.device, "disk4");
    assert!(back.writable);

    // A machine with no IDE port refuses rather than configuring a disk
    // it could never reach, and says which port it wants.
    let mut no_ide = MachineSetup::default();
    no_ide.select_model(Some(MachineModel::A500));
    no_ide.set_host_disks_for_test(vec![HostDiskRow {
        id: "disk4".to_string(),
        fingerprint: None,
        volume: "SanDisk".to_string(),
        size: "31.9 GB".to_string(),
        mounted: Vec::new(),
        writable: false,
        attach: Some(crate::config::HostDiskAttach::IdeMaster),
    }]);
    no_ide.select_host_disk(0);
    assert!(
        !no_ide.host_disk_is_selected("disk4"),
        "there is nowhere on an A500 to put it"
    );
    assert_eq!(
        no_ide.host_disk_warning(),
        Some("Host disk attach requires an A600, A1200, A4000 or SCSI controller")
    );
    assert!(no_ide.mount_host_disks().is_err());
    assert!(no_ide.host_disks_attached().is_empty());

    // And taking it off gives it back to the host.
    let mut reloaded = reloaded;
    assert_eq!(
        reloaded.unmount_host_disk(crate::config::HostDiskAttach::IdeMaster),
        Some("disk4".to_string())
    );
    assert_eq!(reloaded.host_disks_attached().len(), 1);
    assert_eq!(reloaded.to_raw().host_disk.len(), 1);
}

#[test]
fn a_fingerprinted_disk_survives_an_os_ordinal_change() {
    let mut setup = MachineSetup {
        host_disks_attached: vec![crate::config::HostDiskConfig {
            device: "disk4".to_string(),
            fingerprint: Some("v1-stable".to_string()),
            identity_confirmed: false,
            attach: crate::config::HostDiskAttach::IdeMaster,
            writable: false,
        }],
        host_disk_selected: vec!["disk4".to_string()],
        host_disks: vec![HostDiskRow {
            id: "disk9".to_string(),
            fingerprint: Some("v1-stable".to_string()),
            volume: "same medium".to_string(),
            size: "4.0 GB".to_string(),
            mounted: Vec::new(),
            writable: false,
            attach: None,
        }],
        ..Default::default()
    };

    setup.reconcile_host_disk_rows(&[]);

    assert_eq!(setup.host_disks_attached[0].device, "disk9");
    assert!(setup.host_disk_is_selected("disk9"));
    assert_eq!(
        setup.host_disks[0].attach,
        Some(crate::config::HostDiskAttach::IdeMaster)
    );
}

#[test]
fn a_same_ordinal_replacement_inherits_no_disk_authority() {
    let mut setup = MachineSetup {
        host_disks_attached: vec![crate::config::HostDiskConfig {
            device: "disk4".to_string(),
            fingerprint: Some("v1-original".to_string()),
            identity_confirmed: true,
            attach: crate::config::HostDiskAttach::IdeMaster,
            writable: true,
        }],
        host_disk_selected: vec!["disk4".to_string()],
        ..Default::default()
    };
    let previous = vec![HostDiskRow {
        id: "disk4".to_string(),
        fingerprint: Some("v1-original".to_string()),
        volume: "original".to_string(),
        size: "4.0 GB".to_string(),
        mounted: Vec::new(),
        writable: true,
        attach: Some(crate::config::HostDiskAttach::IdeMaster),
    }];
    setup.host_disks = vec![HostDiskRow {
        id: "disk4".to_string(),
        fingerprint: Some("v1-replacement".to_string()),
        volume: "replacement".to_string(),
        size: "4.0 GB".to_string(),
        mounted: Vec::new(),
        writable: false,
        attach: None,
    }];

    setup.reconcile_host_disk_rows(&previous);

    assert!(!setup.host_disk_is_selected("disk4"));
    assert!(!setup.host_disks[0].writable);
    assert_eq!(setup.host_disks[0].attach, None);
    assert_eq!(setup.host_disks_attached[0].device, "disk4");
    assert_eq!(
        setup.host_disks_attached[0].fingerprint.as_deref(),
        Some("v1-original")
    );
}

/// A disk has a place only while it is ticked. Ticking assigns the first
/// free point (IDE Master leads), unticking blanks it again, a choice
/// stepped on a ticked row survives other disks coming and going, and
/// when every place is taken the tick does not happen at all and says
/// why.
#[test]
fn a_place_exists_only_while_the_disk_is_ticked() {
    use crate::config::HostDiskAttach as A;
    let disks = |n: usize| -> Vec<HostDiskRow> {
        (0..n)
            .map(|i| HostDiskRow {
                id: format!("disk{i}"),
                fingerprint: None,
                volume: format!("Card {i}"),
                size: "4.0 GB".to_string(),
                mounted: Vec::new(),
                writable: true,
                attach: None,
            })
            .collect()
    };

    let mut setup = MachineSetup::default();
    setup.select_model(Some(MachineModel::A1200));
    setup.set_host_disks_for_test(disks(3));

    // Blank until ticked, and stepping a blank cell is not a request.
    assert_eq!(setup.host_disks()[0].attach, None);
    setup.cycle_host_disk_attach(0, true);
    assert_eq!(setup.host_disks()[0].attach, None);

    // Ticking assigns the first free point; the next tick the next.
    setup.select_host_disk(0);
    assert_eq!(setup.host_disks()[0].attach, Some(A::IdeMaster));
    setup.select_host_disk(1);
    assert_eq!(setup.host_disks()[1].attach, Some(A::IdeSlave));

    // A choice stepped on a ticked row stands while the disk stays
    // ticked, and unticking another disk frees its place.
    setup.select_host_disk(1);
    assert_eq!(setup.host_disks()[1].attach, None, "unticking blanks it");
    setup.cycle_host_disk_attach(0, true);
    assert_eq!(setup.host_disks()[0].attach, Some(A::IdeSlave));
    setup.select_host_disk(1);
    assert_eq!(
        setup.host_disks()[1].attach,
        Some(A::IdeMaster),
        "the freed place is picked up by the next tick"
    );

    // Nothing left on a machine with no SCSI: the third tick is refused,
    // stays unticked, and the next tick clears the warning.
    setup.select_host_disk(2);
    assert!(!setup.host_disk_is_selected("disk2"));
    assert_eq!(setup.host_disks()[2].attach, None);
    assert_eq!(
        setup.host_disk_warning(),
        Some("Every attachment point is already in use")
    );
    setup.select_host_disk(1);
    assert_eq!(setup.host_disk_warning(), None);
}

/// Taking the controller out takes its disks with it, rather than
/// carrying them to Run to be refused there.
#[test]
fn a_disk_goes_when_the_port_it_was_on_does() {
    let mut setup = MachineSetup::default();
    setup.select_model(Some(MachineModel::A3000));
    setup.set_host_disks_for_test(vec![HostDiskRow {
        id: "disk4".to_string(),
        fingerprint: None,
        volume: "SanDisk".to_string(),
        size: "31.9 GB".to_string(),
        mounted: Vec::new(),
        writable: true,
        attach: None,
    }]);
    // Pick the motherboard controller, which is what makes its units real.
    while setup.scsi_controller_for_test() != Some(ScsiController::A3000) {
        setup.cycle(LauncherField::ScsiController, true);
    }
    setup.select_host_disk(0);
    assert_eq!(
        setup.host_disks()[0].attach,
        Some(crate::config::HostDiskAttach::Scsi(0)),
        "an A3000 has no IDE, so the first free point is a SCSI unit"
    );
    setup.mount_host_disks().expect("the A3000 has SCSI");
    assert_eq!(setup.host_disks_attached().len(), 1);

    // Take the controller away.
    while setup.scsi_controller_for_test().is_some() {
        setup.cycle(LauncherField::ScsiController, true);
    }
    assert!(!setup.has_scsi_controller());
    assert!(
        setup.host_disks_attached().is_empty(),
        "the disk goes with the controller"
    );
    assert!(!setup.host_disk_is_selected("disk4"));
    assert_eq!(
        setup.host_disks()[0].attach,
        None,
        "a disk with no port is going nowhere, and its cell reads blank"
    );
}

/// A real disk takes its place on the Boot Priority page like any other
/// drive, but the priority itself is not ours to set: the partitions on
/// the disk carry their own, and nothing here overrides them.
#[test]
fn a_host_disk_shows_on_boot_priority_with_its_priority_where_it_lives() {
    let mut setup = MachineSetup::default();
    setup.select_model(Some(MachineModel::A1200));
    setup.set_host_disks_for_test(vec![HostDiskRow {
        id: "disk4".to_string(),
        fingerprint: None,
        volume: "SanDisk".to_string(),
        size: "31.9 GB".to_string(),
        mounted: Vec::new(),
        writable: true,
        attach: Some(crate::config::HostDiskAttach::IdeMaster),
    }]);
    // Nothing attached: the row reads as an empty slot.
    assert_eq!(
        setup.disabled_reason(LauncherField::IdeMasterBoot),
        Some("No drive")
    );

    setup.select_host_disk(0);
    setup.mount_host_disks().expect("A1200 has an IDE port");
    assert_eq!(
        setup.disabled_reason(LauncherField::IdeMasterBoot),
        Some("Host Disk")
    );
    assert!(
        !setup.row_hidden(LauncherField::IdeMasterBoot),
        "the drive is there, so its row is too"
    );
}

/// The Interface row carries no driver list of its own: it offers what
/// the library compiled in, in the library's order, so the row leads with
/// the driver that build is meant to use. The starting value is pinned to
/// "None" rather than sampled, because bridging a bay asks the host what
/// is plugged in, and what a test machine has attached is not this test's
/// subject.
#[cfg(feature = "fluxbridge")]
#[test]
fn the_interface_row_offers_the_librarys_drivers_in_its_order() {
    let offered = bridge_drivers();
    let lead = *offered.first().expect("the bridge offers its drivers");
    let library: Vec<&str> = crate::fluxbridge::drivers()
        .iter()
        .map(|driver| driver.token)
        .collect();
    let row: Vec<&str> = offered.iter().map(|d| d.match_token()).collect();
    assert_eq!(row, library, "the row is the library's table, in order");

    let mut setup = MachineSetup::default();
    setup.set_drive_bridged(0, true);
    setup.set_bridge_edit_drive(0);
    setup.df_bridge_none[0] = true;
    assert_eq!(setup.value_label(F::BridgeDevice), "None");
    // One step forward off "None" reaches the first interface offered.
    setup.cycle(F::BridgeDevice, true);
    assert_eq!(setup.value_label(F::BridgeDevice), lead.label());
}

/// Drive speed acts on image bays only, so the row greys exactly when
/// every fitted bay is physical: one image bay anywhere keeps it live.
#[cfg(feature = "fluxbridge")]
#[test]
fn drive_speed_greys_when_every_fitted_bay_is_physical() {
    let mut setup = MachineSetup {
        floppy_drives: 2,
        ..Default::default()
    };
    assert_eq!(setup.disabled_reason(F::FloppySpeed), None);

    setup.set_drive_bridged(0, true);
    assert_eq!(
        setup.disabled_reason(F::FloppySpeed),
        None,
        "df1 is still an image bay"
    );

    setup.set_drive_bridged(1, true);
    assert!(
        setup.disabled_reason(F::FloppySpeed).is_some(),
        "every fitted bay is physical"
    );

    // An unfitted bay's state does not count: df2 is not wired in.
    setup.floppy_drives = 3;
    assert_eq!(
        setup.disabled_reason(F::FloppySpeed),
        None,
        "df2 is a fitted image bay again"
    );
}

/// The bridge page follows what is there: with nothing attached only the
/// Interface row stays live, and with the bay pulled out from under the
/// page (a loaded config can do that) every row greys, Interface included.
/// With an interface attached the rows answer to the driver as before.
#[cfg(feature = "fluxbridge")]
#[test]
fn bridge_page_greys_without_an_interface() {
    let mut setup = MachineSetup::default();
    setup.set_drive_bridged(0, true);
    setup.set_bridge_edit_drive(0);
    // The port row answers to its own rules (tested below): it must stay
    // pickable for an interface the library's scan cannot name.
    let all = [
        F::BridgeCable,
        F::BridgeDensity,
        F::BridgeReadMode,
        F::BridgeReplaySpeed,
    ];

    setup.df_bridge_none[0] = false;
    setup.bridge_status = BridgeStatus::NoInterface;
    assert_eq!(setup.disabled_reason(F::BridgeDevice), None);
    for f in all {
        assert!(
            setup.disabled_reason(f).is_some(),
            "{f:?} live with no interface"
        );
    }

    setup.bridge_status = BridgeStatus::Attached;
    assert_eq!(setup.disabled_reason(F::BridgeDevice), None);
    for f in [F::BridgeDensity, F::BridgeReadMode, F::BridgeReplaySpeed] {
        assert_eq!(setup.disabled_reason(f), None, "{f:?} greyed with one");
    }

    // An interface of "None" greys the page whatever is attached, the
    // port row included.
    setup.df_bridge_none[0] = true;
    assert_eq!(setup.disabled_reason(F::BridgeDevice), None);
    for f in all {
        assert!(
            setup.disabled_reason(f).is_some(),
            "{f:?} live with interface None"
        );
    }
    assert!(setup.disabled_reason(F::BridgePort).is_some());
    setup.df_bridge_none[0] = false;

    // The port row greys exactly when there is nothing to point at:
    // a list of just "Automatic". Pinned, not sampled -- the machine
    // running this test has its own serial devices.
    setup.bridge_ports = vec![None];
    assert!(setup.disabled_reason(F::BridgePort).is_some());
    setup.bridge_ports = vec![None, Some("/dev/cu.wchusbserial1420".to_string())];
    assert_eq!(setup.disabled_reason(F::BridgePort), None);

    // The bay un-bridged underneath the page: nothing left to edit.
    setup.set_drive_bridged(0, false);
    assert!(setup.disabled_reason(F::BridgeDevice).is_some());
    for f in all {
        assert!(setup.disabled_reason(f).is_some(), "{f:?} live with no bay");
    }
}

/// Drive select is shaped by the interface, so it only answers to one
/// when there is one: attached and chosen. That is what separates a row
/// greyed with its steppers (this interface has no drive-select line)
/// from one blanked entirely (there is no interface to ask).
#[cfg(feature = "fluxbridge")]
#[test]
fn drive_select_answers_to_an_interface_only_when_there_is_one() {
    let mut setup = MachineSetup::default();
    setup.set_drive_bridged(0, true);
    setup.set_bridge_edit_drive(0);

    setup.bridge_status = BridgeStatus::NoInterface;
    setup.df_bridge_none[0] = false;
    assert!(!setup.bridge_interface_selected(), "nothing attached");

    setup.bridge_status = BridgeStatus::Attached;
    setup.df_bridge_none[0] = true;
    assert!(!setup.bridge_interface_selected(), "interface set to None");

    setup.df_bridge_none[0] = false;
    assert!(setup.bridge_interface_selected(), "attached and chosen");

    setup.set_drive_bridged(0, false);
    assert!(!setup.bridge_interface_selected(), "no bay at all");
}

/// A bridged bay only names its interface when one is actually attached:
/// with nothing plugged in the media row says so, whatever the bay is
/// configured for.
// A build without the feature has no bridges to configure: the keys are
// read and ignored, so there is nothing here to assert.
#[cfg(feature = "fluxbridge")]
#[test]
fn a_bridged_bay_names_its_interface_only_when_one_is_attached() {
    let mut setup = MachineSetup::default();
    setup.set_drive_bridged(0, true);
    setup.df_bridge_none[0] = false;

    setup.bridge_status = BridgeStatus::Attached;
    assert_eq!(
        setup.drive_bridge_label(0),
        crate::config::BridgeDriver::default().label()
    );

    setup.bridge_status = BridgeStatus::NoInterface;
    assert_eq!(setup.drive_bridge_label(0), "Not connected");

    // An interface of "None" reads the same: nothing is on the bay
    // either way.
    setup.bridge_status = BridgeStatus::Attached;
    setup.df_bridge_none[0] = true;
    assert_eq!(setup.drive_bridge_label(0), "Not connected");

    // An image-backed bay is not a bridge at all, connected or not.
    assert_eq!(setup.drive_bridge_label(1), "(none)");
}

/// An interface of "None" keeps the tick box and the page, but the built
/// config -- what a run uses and what a save writes -- carries no bridge:
/// the bay is effectively unbridged until an interface is chosen.
#[cfg(feature = "fluxbridge")]
#[test]
fn a_none_interface_builds_an_unbridged_bay() {
    let mut setup = MachineSetup::default();
    setup.set_drive_bridged(0, true);
    setup.df_bridge_none[0] = true;
    let cfg = setup.build_config().expect("valid");
    assert!(cfg.floppy.bridges[0].is_none(), "no bridge in the config");

    setup.df_bridge_none[0] = false;
    let cfg = setup.build_config().expect("valid");
    assert!(
        cfg.floppy.bridges[0].is_some(),
        "an interface brings it back"
    );
}

/// "Automatic" leads, the library's scan keeps its order, and host
/// devices join only when the scan did not already name them -- a macOS
/// `cu.` device counts as named when its `tty.` twin is listed.
#[cfg(feature = "fluxbridge")]
#[test]
fn port_lists_merge_without_duplicates() {
    let merged = merge_port_lists(
        vec!["/dev/tty.usbmodem1101".to_string()],
        vec![
            "/dev/cu.Bluetooth-Incoming-Port".to_string(),
            "/dev/cu.usbmodem1101".to_string(),
            "/dev/cu.wchusbserial1420".to_string(),
        ],
    );
    assert_eq!(
        merged,
        vec![
            None,
            Some("/dev/tty.usbmodem1101".to_string()),
            Some("/dev/cu.Bluetooth-Incoming-Port".to_string()),
            Some("/dev/cu.wchusbserial1420".to_string()),
        ]
    );
}

/// The write-protect box governs a real drive as well as an image, and it
/// starts ticked: a bay handed a physical disk must not come up writable
/// because nobody said otherwise.
#[cfg(feature = "fluxbridge")]
#[test]
fn dropping_a_drive_releases_the_physical_one_it_was_holding() {
    let mut setup = MachineSetup {
        floppy_drives: 2,
        ..Default::default()
    };
    setup.set_drive_bridged(1, true);
    assert!(setup.df_bridge[1].is_some(), "df1 bridged");

    // Take the machine back to one drive. df1's row goes off the page, so
    // if it kept the bridge nothing would explain why the interface was
    // busy the next time a bay asked for it.
    setup.cycle(F::FloppyDrives, false);
    assert_eq!(setup.floppy_drives, 1);
    assert!(setup.df_bridge[1].is_none(), "df1 released the drive");

    // df0 is still fitted, so anything set on it stays put.
    setup.set_drive_bridged(0, true);
    setup.floppy_drives = 2;
    setup.cycle(F::FloppyDrives, false);
    assert!(setup.df_bridge[0].is_some(), "df0 kept its bridge");
}

// A build without the feature has no bridges to configure: the keys are
// read and ignored, so there is nothing here to assert.
#[cfg(feature = "fluxbridge")]
#[test]
fn write_protect_governs_a_bridged_bay_and_survives_a_round_trip() {
    let mut setup = MachineSetup::default();
    setup.set_drive_bridged(0, true);
    // Pin an interface: a hardware-less host auto-selects "None", which
    // deliberately keeps the bridge out of the built config.
    setup.df_bridge_none[0] = false;
    assert!(
        setup.toggle_value(F::Df0WriteProtect),
        "protected by default"
    );

    let cfg = setup.build_config().expect("valid");
    let bridge = cfg.floppy.bridges[0].as_ref().expect("bridged");
    assert!(bridge.write_protected);

    // Untick it, and both the emitted config and the bay's own copy follow.
    setup.toggle(F::Df0WriteProtect);
    assert!(!setup.toggle_value(F::Df0WriteProtect));
    assert!(
        !setup.df_bridge[0]
            .as_ref()
            .expect("bridged")
            .write_protected
    );
    let cfg = setup.build_config().expect("valid");
    assert!(
        !cfg.floppy.bridges[0]
            .as_ref()
            .expect("bridged")
            .write_protected
    );

    // And it comes back the same way, rather than reverting to protected.
    let reloaded = MachineSetup::from_raw(&setup.to_raw()).expect("round trip");
    assert!(!reloaded.toggle_value(F::Df0WriteProtect));
    assert!(
        !reloaded.df_bridge[0]
            .as_ref()
            .expect("bridged")
            .write_protected
    );
}

fn write_board_manifest() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "copperline-launcher-board-{}-{}.toml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(
        &path,
        r#"
            name = "Test Plugin"
            zorro = 2
            type = "wasm"
            size = "64K"
            manufacturer = 5192
            product = 16
            wasm = "x.wasm"
            [config]
            speed = "fast"
            verbose = false
            [[option]]
            key = "speed"
            label = "Speed"
            type = "enum"
            choices = ["slow", "fast"]
            [[option]]
            key = "verbose"
            label = "Verbose"
            type = "bool"
            [[option]]
            key = "count"
            label = "Count"
            type = "int"
            default = 3
            [[option]]
            key = "rom"
            label = "ROM"
            type = "file"
        "#,
    )
    .unwrap();
    path
}

#[test]
fn plugin_board_options_load_edit_and_round_trip() {
    let path = write_board_manifest();
    let mut board = ZorroBoardSetup::load(path.clone());
    assert_eq!(board.options().len(), 4);
    // Defaults: [config] for speed/verbose, the option default for count.
    assert_eq!(board.value(0), "fast");
    assert_eq!(board.value(1), "false");
    assert_eq!(board.value(2), "3");
    assert_eq!(board.value(3), ""); // unset file

    board.cycle(0, true); // enum fast -> slow (wraps)
    assert_eq!(board.value(0), "slow");
    board.toggle(1); // bool false -> true
    assert_eq!(board.value(1), "true");
    board.cycle(2, false); // int 3 -> 2
    assert_eq!(board.value(2), "2");
    board.set(3, "/tmp/board.rom".into());
    assert_eq!(board.value(3), "/tmp/board.rom");
    board.clear(2); // revert int to its default
    assert_eq!(board.value(2), "3");

    // Overrides serialize back, typed per the option schema.
    let setup = MachineSetup {
        zorro_boards: vec![board],
        ..MachineSetup::default()
    };
    let raw = setup.to_raw();
    let cfg = raw.zorro[0].config.as_ref().expect("overrides emitted");
    assert_eq!(cfg.get("speed").unwrap().as_str(), Some("slow"));
    assert_eq!(cfg.get("verbose").unwrap().as_bool(), Some(true));
    assert_eq!(cfg.get("rom").unwrap().as_str(), Some("/tmp/board.rom"));
    // "count" was reverted to default, so it is not emitted.
    assert!(cfg.get("count").is_none());

    // And those overrides round-trip back through from_raw.
    let reloaded = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(reloaded.zorro_boards()[0].value(0), "slow");
    assert_eq!(reloaded.zorro_boards()[0].value(1), "true");

    let _ = std::fs::remove_file(&path);
}

fn raw_mount(path: &str) -> RawFilesysMount {
    RawFilesysMount {
        path: path.to_string(),
        volume: Some(path.trim_start_matches('/').to_uppercase()),
        bootpri: None,
        readonly: None,
    }
}

#[test]
fn host_mounts_round_trip_and_keep_entries_past_the_gui_slots() {
    let mut raw = RawConfig {
        filesys: (0..6).map(|i| raw_mount(&format!("/host{i}"))).collect(),
        ..RawConfig::default()
    };
    // A hand-written readonly flag on a GUI-slot mount must survive a save.
    raw.filesys[0].readonly = Some(true);

    let mut setup = MachineSetup::from_raw(&raw).unwrap();
    // The GUI edits the first FILESYS_GUI_SLOTS mounts; the rest are held
    // verbatim so a save never drops a hand-written entry.
    assert_eq!(setup.filesys_dirs[0], Some(PathBuf::from("/host0")));
    assert_eq!(setup.filesys_dirs[3], Some(PathBuf::from("/host3")));
    assert_eq!(setup.filesys_extra.len(), 2);

    // An untouched save is a faithful round trip.
    assert_eq!(setup.to_raw().filesys, raw.filesys);

    // The Access spinner flips between the two modes; a writable mount
    // emits no readonly key at all rather than an explicit false.
    assert_eq!(
        setup.value_label(LauncherField::Filesys0ReadOnly),
        "Read-only"
    );
    setup.cycle(LauncherField::Filesys0ReadOnly, true);
    assert_eq!(
        setup.value_label(LauncherField::Filesys0ReadOnly),
        "Read-write"
    );
    assert_eq!(setup.to_raw().filesys[0].readonly, None);
    setup.cycle(LauncherField::Filesys0ReadOnly, false);
    assert_eq!(setup.to_raw().filesys[0].readonly, Some(true));

    // Clearing a slot removes that mount. HOSTFS<n> is the position in the
    // config, so the mounts after it renumber, exactly as they would if the
    // entry were deleted from the TOML by hand.
    setup.filesys_dirs[1] = None;
    setup.filesys_names[1] = None;
    let saved = setup.to_raw().filesys;
    let paths: Vec<&str> = saved.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(paths, ["/host0", "/host2", "/host3", "/host4", "/host5"]);

    // The formerly-extra mounts now fall inside the GUI slots, and the
    // volume names still travel with their own paths.
    let back = MachineSetup::from_raw(&RawConfig {
        filesys: saved,
        ..RawConfig::default()
    })
    .unwrap();
    assert_eq!(back.filesys_dirs[1], Some(PathBuf::from("/host2")));
    assert_eq!(back.filesys_names[1].as_deref(), Some("HOST2"));
    assert_eq!(back.filesys_extra.len(), 1);
}

#[test]
fn whdload_settings_round_trip_including_args() {
    // All four [whdload] keys, args included: args has no UI row, so it
    // must be carried through rather than edited.
    let raw = RawConfig {
        whdload: crate::config::RawWhdload {
            game: Some("/games/Turrican.lha".to_string()),
            library: Some("/whd/library".to_string()),
            kickstarts: Some("/roms/kickstarts".to_string()),
            args: Some("NoVBRMove ButtonWait".to_string()),
            ..crate::config::RawWhdload::default()
        },
        ..RawConfig::default()
    };
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(
        setup.path(F::WhdloadGame),
        Some(Path::new("/games/Turrican.lha"))
    );
    assert_eq!(
        setup.path(F::WhdloadKickstarts),
        Some(Path::new("/roms/kickstarts"))
    );
    assert_eq!(
        setup.path(F::WhdloadLibrary),
        Some(Path::new("/whd/library"))
    );
    assert_eq!(setup.to_raw().whdload, raw.whdload);
}

/// Every Paths row must reach an entry, resolve to a directory, and
/// come back out in `[paths]`. Three separate matches stand between a
/// row and its entry -- one to read, one to write, one to resolve -- so
/// this walks all of them for every row on the page: a row wired to the
/// wrong entry, or to none, cannot pass.
#[test]
fn every_paths_row_reaches_its_own_entry() {
    // `set_path` on a Paths row adopts, which writes the process-wide
    // store another test asserts against.
    let _guard = crate::paths::adopted_store_lock();
    let mut setup = MachineSetup::default();
    let fields: Vec<LauncherField> = PATHS_ROWS
        .iter()
        .map(|row| row.field)
        .filter(|field| field.is_paths_field())
        .collect();
    assert_eq!(fields.len(), 11, "every entry should have had a row");
    for (i, &field) in fields.iter().enumerate() {
        assert!(
            !setup.paths_is_set(field),
            "{field:?} starts out set, so nothing here proves anything"
        );
        let dir = PathBuf::from(format!("/probe/{i}"));
        setup.set_path(field, dir.clone());
        assert_eq!(
            setup.path(field),
            Some(dir.as_path()),
            "{field:?} does not read back what it was given"
        );
        assert!(setup.paths_is_set(field), "{field:?} still reads inherited");
    }
    // Resolution is checked only once every row is set. Checked as each
    // one is set, a row resolving through a *later* row's entry would
    // still land on that row's untouched default and look fine.
    let mut seen: Vec<PathBuf> = Vec::new();
    for &field in &fields {
        let resolved = setup
            .paths_resolved(field)
            .expect("a Paths row resolves to a directory");
        assert!(
            !seen.contains(&resolved),
            "{field:?} resolves to {resolved:?}, which another row already claimed"
        );
        seen.push(resolved);
    }
    // Out through the configuration and back, since that is the only
    // way any of it survives.
    let round_tripped = MachineSetup::from_raw(&setup.to_raw()).expect("valid");
    for row in PATHS_ROWS {
        if row.field.is_paths_field() {
            assert_eq!(
                round_tripped.path(row.field),
                setup.path(row.field),
                "{:?} did not survive [paths]",
                row.field
            );
        }
    }
    // And cleared, each one goes back to inheriting rather than to a
    // frozen copy of today's default.
    for row in PATHS_ROWS {
        if row.field.is_paths_field() {
            setup.clear_path(row.field);
        }
    }
    assert!(
        setup.to_raw().paths.is_empty(),
        "a cleared page emits no [paths]"
    );
}

/// A machine configuration that never mentions directories must not
/// grow a `[paths]` section just by passing through the launcher.
#[test]
fn an_untouched_paths_page_writes_nothing() {
    let raw = MachineSetup::default().to_raw();
    assert!(raw.paths.is_empty());
    let text = raw.to_toml_string().expect("serialises");
    assert!(!text.contains("[paths]"), "{text}");
}

#[test]
fn whdload_paths_route_through_set_path_and_clear_path() {
    let mut setup = MachineSetup::default();
    for field in [F::WhdloadGame, F::WhdloadKickstarts, F::WhdloadLibrary] {
        assert_eq!(setup.path(field), None);
    }
    // Distinct paths per field, so cross-wired slots would show.
    setup.set_path(F::WhdloadGame, PathBuf::from("/g/game.lha"));
    setup.set_path(F::WhdloadKickstarts, PathBuf::from("/k"));
    setup.set_path(F::WhdloadLibrary, PathBuf::from("/l"));
    assert_eq!(setup.path(F::WhdloadGame), Some(Path::new("/g/game.lha")));
    assert_eq!(setup.path(F::WhdloadKickstarts), Some(Path::new("/k")));
    assert_eq!(setup.path(F::WhdloadLibrary), Some(Path::new("/l")));
    let raw = setup.to_raw();
    assert_eq!(raw.whdload.game.as_deref(), Some("/g/game.lha"));
    assert_eq!(raw.whdload.kickstarts.as_deref(), Some("/k"));
    assert_eq!(raw.whdload.library.as_deref(), Some("/l"));
    // The WHDLoad volumes have fixed names, so no path row here ever
    // grows a volume box -- an editable name with nothing to name.
    // Every one of them, so a row added later is not left with a box
    // that cannot be typed into.
    for field in [
        F::WhdloadGame,
        F::WhdloadGames,
        F::WhdloadKickstarts,
        F::WhdloadLibrary,
        F::WhdloadWhdPackage,
        F::WhdloadSkickPackage,
    ] {
        setup.set_path(field, PathBuf::from("/somewhere"));
        assert!(
            !setup.drive_name_applies(field),
            "{field:?} offers a volume box"
        );
    }
    for field in [
        F::WhdloadGame,
        F::WhdloadGames,
        F::WhdloadKickstarts,
        F::WhdloadLibrary,
        F::WhdloadWhdPackage,
        F::WhdloadSkickPackage,
    ] {
        setup.clear_path(field);
        assert_eq!(setup.path(field), None);
    }
    assert_eq!(setup.to_raw().whdload, crate::config::RawWhdload::default());
}

#[test]
fn the_rom_tab_carries_an_identification_line_under_each_path_row() {
    // Each ROM path row is followed by a note row keyed on the same
    // field, so the identification is hidden and greyed with its row.
    let rows = rows(
        LauncherTab::Rom,
        ParallelDevice::None,
        SerialMode::Off,
        false,
        false,
    );
    let shape: Vec<(&str, RowKind, LauncherField)> =
        rows.iter().map(|r| (r.label, r.kind, r.field)).collect();
    assert_eq!(
        shape,
        [
            ("Primary ROM:", RowKind::SectionHeader, F::SectionHeader),
            ("  Kickstart ROM", RowKind::Path, F::Rom),
            ("Name", RowKind::RomInfo, F::Rom),
            ("Version", RowKind::RomInfo, F::Rom),
            ("Revision", RowKind::RomInfo, F::Rom),
            ("Extended ROM:", RowKind::SectionHeader, F::SectionHeader),
            ("  Extended ROM", RowKind::Path, F::ExtendedRom),
            (
                "CD32 Full Motion Video:",
                RowKind::SectionHeader,
                F::SectionHeader,
            ),
            ("  FMV module ROM", RowKind::Path, F::FmvRom),
        ]
    );

    // The identification splits into its three facts; the models
    // after the revision are dropped.
    let mut probe = LauncherState::new(MachineSetup::default());
    probe.set_rom_note_for_test(F::Rom, "Kickstart 3.1 (40.68) A1200");
    assert_eq!(
        probe.rom_note_cells(F::Rom),
        (
            "Kickstart".to_string(),
            "3.1".to_string(),
            "40.68".to_string()
        )
    );
    // A parenthesized variant is not a revision.
    probe.set_rom_note_for_test(F::Rom, "Kickstart 1.0 A1000 (NTSC)");
    assert_eq!(
        probe.rom_note_cells(F::Rom),
        ("Kickstart".to_string(), "1.0".to_string(), String::new())
    );
    // The bundled AROS carries numbers read off the image itself,
    // so they follow releases.
    let bundled = LauncherState::new(MachineSetup::default());
    let (name, version, revision) = bundled.rom_note_cells(F::Rom);
    assert_eq!(name, "AROS");
    assert!(
        !version.is_empty() && !revision.is_empty(),
        "the AROS image carries its own numbers"
    );
    // An unrecognised image leaves the values blank.
    let mut setup = MachineSetup::default();
    setup.set_path(F::Rom, std::path::PathBuf::from("mystery-dump.rom"));
    let unknown = LauncherState::new(setup);
    assert_eq!(
        unknown.rom_note_cells(F::Rom),
        (String::new(), String::new(), String::new())
    );

    let mut state = LauncherState::new(MachineSetup::default());
    // Nothing chosen: the value column says the machine boots the
    // bundled AROS, and there is no image to identify.
    assert_eq!(state.setup.value_label(F::Rom), "(bundled AROS)");
    assert_eq!(state.rom_note(F::Rom), None);
    assert_eq!(state.rom_note(F::ExtendedRom), None);

    // A file that is not a known ROM (and here does not exist at all)
    // leaves the line blank rather than claiming anything.
    state
        .setup
        .set_path(F::Rom, PathBuf::from("/roms/mystery.rom"));
    state.sync_rom_notes();
    assert_eq!(state.rom_note(F::Rom), None);

    // A recognised image: the note names the Kickstart, and the value
    // column still shows the file the user picked.
    state.set_rom_note_for_test(F::Rom, "Kickstart 3.1 (40.68) A1200");
    assert_eq!(state.setup.value_label(F::Rom), "mystery.rom");
    assert_eq!(state.rom_note(F::Rom), Some("Kickstart 3.1 (40.68) A1200"));
    // The identification is cached against the path: syncing again with
    // the same file in the field must not read (and so re-identify) it.
    state.sync_rom_notes();
    assert_eq!(state.rom_note(F::Rom), Some("Kickstart 3.1 (40.68) A1200"));
    // Choosing another file does re-identify, and clearing the row drops
    // the note with it.
    state
        .setup
        .set_path(F::Rom, PathBuf::from("/roms/other.rom"));
    state.sync_rom_notes();
    assert_eq!(state.rom_note(F::Rom), None);
    state.set_rom_note_for_test(F::Rom, "Kickstart 1.3 (34.5) A500/A1000/A2000");
    state.setup.clear_path(F::Rom);
    state.sync_rom_notes();
    assert_eq!(state.rom_note(F::Rom), None);
    // The extended ROM keeps its own line.
    assert_eq!(state.rom_note(F::ExtendedRom), None);
    state
        .setup
        .set_path(F::ExtendedRom, PathBuf::from("/roms/cd32-ext.rom"));
    state.set_rom_note_for_test(F::ExtendedRom, "CD32 extended ROM (40.60)");
    assert_eq!(state.rom_note(F::Rom), None);
    assert_eq!(
        state.rom_note(F::ExtendedRom),
        Some("CD32 extended ROM (40.60)")
    );
    // Only the ROM rows have one.
    assert_eq!(state.rom_note(F::Df0Image), None);
}

#[test]
fn fmv_rom_path_round_trips_through_the_launcher() {
    let mut setup = MachineSetup::default();
    setup.select_model(Some(MachineModel::Cd32));
    assert_eq!(setup.value_label(F::FmvRom), "(bundled open FMV ROM)");
    assert_eq!(setup.to_raw().fmv_rom, None);
    let path = PathBuf::from("/roms/cd32fmv.rom");
    setup.set_path(F::FmvRom, path.clone());
    assert_eq!(setup.path(F::FmvRom), Some(path.as_path()));
    assert_eq!(setup.to_raw().fmv_rom.as_deref(), Some("/roms/cd32fmv.rom"));

    setup.clear_path(F::FmvRom);
    assert_eq!(setup.path(F::FmvRom), None);
    assert_eq!(setup.to_raw().fmv_rom, None);

    setup.set_path(F::FmvRom, path);
    setup.select_model(Some(MachineModel::A1200));
    assert_eq!(setup.path(F::FmvRom), None, "non-CD32 profile drops module");
}

#[test]
fn fmv_rom_explicit_opt_out_survives_a_launcher_round_trip() {
    let raw = RawConfig::parse(
        r#"
        fmv_rom = ""
        [machine]
        profile = "CD32"
        "#,
    )
    .unwrap();
    let mut setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.path(F::FmvRom), None);
    assert_eq!(setup.value_label(F::FmvRom), "(no FMV module)");
    assert_eq!(setup.to_raw().fmv_rom.as_deref(), Some(""));

    setup.set_path(F::FmvRom, PathBuf::from("replacement.rom"));
    assert_eq!(setup.to_raw().fmv_rom.as_deref(), Some("replacement.rom"));
}

#[test]
fn fmv_module_action_switches_between_an_empty_slot_and_the_bundled_default() {
    let mut setup = MachineSetup::default();
    setup.select_model(Some(MachineModel::Cd32));

    setup.toggle_fmv_module();
    assert!(setup.fmv_rom_disabled());
    assert_eq!(setup.value_label(F::FmvRom), "(no FMV module)");
    assert_eq!(setup.to_raw().fmv_rom.as_deref(), Some(""));

    setup.toggle_fmv_module();
    assert!(!setup.fmv_rom_disabled());
    assert_eq!(setup.value_label(F::FmvRom), "(bundled open FMV ROM)");
    assert_eq!(setup.to_raw().fmv_rom, None);

    setup.set_path(F::FmvRom, PathBuf::from("replacement.rom"));
    setup.toggle_fmv_module();
    assert_eq!(setup.path(F::FmvRom), None);
    assert_eq!(setup.to_raw().fmv_rom.as_deref(), Some(""));
}

#[test]
fn the_machine_type_cycle_reports_which_machine_it_means() {
    use crate::config::WhdloadMachine as M;
    let mut setup = MachineSetup::default();
    // Auto is the default: the slave header says what the game wants.
    assert_eq!(setup.whdload_machine(), M::Auto);
    assert_eq!(setup.value_label(F::WhdloadMachine), "Auto");

    // The two settings are one cycle apart either way round, so the
    // reading after a press is the reading the line describes.
    setup.cycle(F::WhdloadMachine, true);
    assert_eq!(setup.whdload_machine(), M::Copperline);
    assert_eq!(setup.value_label(F::WhdloadMachine), "Copperline");
    setup.cycle(F::WhdloadMachine, true);
    assert_eq!(setup.whdload_machine(), M::Auto);
    setup.cycle(F::WhdloadMachine, false);
    assert_eq!(setup.whdload_machine(), M::Copperline, "and backwards");
}

#[cfg(feature = "game-library")]
#[test]
fn a_store_from_another_folder_is_not_this_folders_list() {
    use crate::gamelib::Known;
    // A store carrying a collection listed somewhere else -- which is
    // what a fresh configuration pointed at a new folder meets.
    let mut state = LauncherState::new(MachineSetup::default());
    state.library.db.set_known(
        (0..40)
            .map(|at| Known {
                file: format!("Old{at}.lha"),
                game: None,
                manual: false,
                slave_sha1: None,
            })
            .collect(),
    );
    state.library.db.set_folder(Path::new("/games/old"));
    state.library.db_loaded = true;

    // Pointed at a different folder, the page offers none of them: they
    // are paths under somewhere else and none of them is here.
    state
        .setup
        .set_path(F::WhdloadGames, PathBuf::from("/games/new"));
    state.refresh_library(Path::new("/nonexistent"));
    assert!(
        state.library.games.is_empty(),
        "another folder's games were listed as this one's"
    );

    // Once the store says it lists this folder, they are its list.
    state.library.db.set_folder(Path::new("/games/new"));
    state.refresh_library(Path::new("/nonexistent"));
    assert_eq!(state.library.games.len(), 40);
}

#[cfg(feature = "game-library")]
#[test]
fn the_az_row_files_games_and_jumps_to_them() {
    use crate::gamelib::{Game, Known, Library};
    assert_eq!(az_label(0), "0-9");
    assert_eq!(az_label(1), "#");
    assert_eq!(az_label(2), "A");
    assert_eq!(az_label(AZ_BUCKETS - 1), "Z");

    // Filed by the first character of the name shown, whatever case.
    assert_eq!(az_bucket_of("Turrican"), az_bucket_of("turrican"));
    assert_eq!(az_bucket_of("Zool"), AZ_BUCKETS - 1);
    assert_eq!(az_bucket_of("1943"), 0);
    assert_eq!(az_bucket_of("+4 Bonus"), 1, "neither letter nor digit");
    // Folded the way the panel draws it: "Élite" reads as "Elite" and
    // sorts among the E's, so it answers to E.
    assert_eq!(az_bucket_of("Élite"), az_bucket_of("Elite"));
    assert_eq!(az_bucket_of("Ätzen"), az_bucket_of("Atzen"));
    // Something with no ASCII at all still has somewhere to go.
    assert_eq!(az_bucket_of("中"), 1);
    assert_eq!(az_bucket_of(""), 1, "nothing to file it under");

    let named = |name: &str| {
        Some(Game {
            name: name.to_string(),
            ..Game::default()
        })
    };
    let known = |file: &str, name: &str| Known {
        file: file.to_string(),
        game: named(name),
        manual: false,
        slave_sha1: None,
    };
    let mut state = LauncherState::new(MachineSetup::default());
    state.library.db.set_known(vec![
        known("a.lha", "Alien Breed"),
        known("t1.lha", "Turrican"),
        known("t2.lha", "Turrican II"),
        known("z.lha", "Zool"),
    ]);
    state.library.games = Library::known(Path::new("/games"), &state.library.db);

    let present = state.az_buckets_present();
    assert!(present[az_bucket_of("Alien Breed")]);
    assert!(present[az_bucket_of("Turrican")]);
    assert!(!present[0], "no game starts with a digit");
    assert!(!present[az_bucket_of("Xenon")], "nothing under X");

    // Jumping lands on the first of that letter, and chooses it.
    state.jump_to_bucket(az_bucket_of("Turrican"), 2);
    let at = state.library.selected;
    assert_eq!(state.library.games.entries()[at].title(), "Turrican");
    assert_eq!(state.library.scroll, at, "it is put at the top of the box");

    // A letter with nothing under it does nothing at all.
    let before = state.library.selected;
    state.jump_to_bucket(az_bucket_of("Xenon"), 2);
    assert_eq!(state.library.selected, before);
}

#[cfg(feature = "game-library")]
#[test]
fn a_version_is_offered_only_for_a_named_game_the_library_holds_twice() {
    use crate::gamelib::{Game, Known, Library};
    let named = |name: &str| {
        Some(Game {
            name: name.to_string(),
            year: Some("1994".to_string()),
            ..Game::default()
        })
    };
    let known = |file: &str, game: Option<Game>| Known {
        file: file.to_string(),
        game,
        manual: false,
        slave_sha1: None,
    };
    let mut state = LauncherState::new(MachineSetup::default());
    state.library.db.set_known(vec![
        // The same game packed twice, which is what a version is for.
        known("CannonFodder2_v1.11_0104.lha", named("Cannon Fodder 2")),
        known("CannonFodder2_v1.12_Fr_2578.zip", named("Cannon Fodder 2")),
        // Held once: nothing to tell apart.
        known("Turrican_v1.3_0087.lha", named("Turrican")),
        // Two the scan could not name. They share a title only because
        // the title falls back to the file name.
        known("Mystery/Thing.lha", None),
        known("Elsewhere/Thing.zip", None),
    ]);
    state.library.games = Library::known(Path::new("/games"), &state.library.db);
    state.library.db_loaded = true;

    let version_of = |state: &mut LauncherState, title: &str| {
        let at = state
            .library
            .games
            .entries()
            .iter()
            .position(|e| e.title() == title)
            .expect("the entry is in the library");
        state.select_library_game(at);
        assert!(state.open_meta_editor(), "the editor opens");
        let value = state
            .meta
            .as_ref()
            .unwrap()
            .value(MetaField::Version)
            .to_string();
        state.meta = None;
        value
    };

    // The package's own name, and not how it was packed: `.lha`
    // against `.zip` is the same on both and says nothing about which
    // release either one is.
    assert_eq!(
        version_of(&mut state, "Cannon Fodder 2"),
        "CannonFodder2_v1.11_0104"
    );
    // Held once, so there is nothing to separate it from.
    assert_eq!(version_of(&mut state, "Turrican"), "");
    // Unnamed by the scan: a file name under a row that already says
    // nothing is not the answer to which release it is.
    assert_eq!(version_of(&mut state, "Thing"), "");
}

#[test]
fn a_held_scroll_climbs_five_stages_and_starts_again_when_let_go() {
    use std::time::{Duration, Instant};
    // What a held button does: a step every 60 ms, which is what the
    // stages have to be read against -- they are a second of holding
    // each, not a count of steps.
    const EVERY: Duration = Duration::from_millis(60);
    let start = Instant::now();
    let mut rate = ScrollRate::default();

    // The first step of a run always moves one row, so a click to nudge
    // the list by one does that and nothing more.
    assert_eq!(rate.rows_for_step(start), 1);

    // Held, it works through a stage a second. Sampled just after each
    // second, which is where the stage that second belongs to shows.
    let mut at = start;
    let mut seen = Vec::new();
    for second in 0..5 {
        let until = start + Duration::from_millis(second * 1000 + 500);
        let mut rows = 0;
        while at < until {
            rows = rate.rows_for_step(at);
            at += EVERY;
        }
        seen.push(rows);
    }
    assert_eq!(seen, vec![1, 3, 7, 14, 24]);

    // The last stage is the ceiling: holding it longer does not keep
    // making it faster.
    for _ in 0..100 {
        assert_eq!(rate.rows_for_step(at), 24, "past the last stage");
        at += EVERY;
    }

    // Letting go and pressing again starts from the bottom: a gap
    // longer than a repeat says the run ended...
    at += Duration::from_millis(400);
    assert_eq!(rate.rows_for_step(at), 1, "a pause ended the run");

    // ...and so does a press that says so outright, which is what the
    // mouse does, since a quick re-click can land inside that gap.
    for _ in 0..80 {
        at += EVERY;
        rate.rows_for_step(at);
    }
    assert!(rate.rows_for_step(at) > 1, "mid-run");
    rate.reset();
    assert_eq!(rate.rows_for_step(at), 1);
}

#[test]
fn a_caret_edits_where_it_stands() {
    let mut text = "Golden Axe".to_string();
    let mut caret = Caret::end_of(&text);
    assert_eq!(caret.at(), 10);

    // Typing goes in at the caret and the caret steps over it.
    caret.left();
    caret.left();
    caret.left();
    for c in "the ".chars() {
        caret.insert(&mut text, c);
    }
    assert_eq!(text, "Golden the Axe");
    assert_eq!(caret.at(), 11);

    // Backspace takes what is behind, Delete what is under, and
    // neither moves the other's way.
    assert!(caret.backspace(&mut text));
    assert_eq!(text, "Golden theAxe");
    assert_eq!(caret.at(), 10);
    assert!(caret.delete(&mut text));
    assert_eq!(text, "Golden thexe");
    assert_eq!(caret.at(), 10, "delete leaves the caret where it was");

    // Neither runs off its end.
    caret.home();
    assert!(!caret.backspace(&mut text));
    caret.end(&text);
    assert!(!caret.delete(&mut text));
    assert_eq!(text, "Golden thexe");

    // Stepping stops at both ends rather than wrapping.
    caret.home();
    caret.left();
    assert_eq!(caret.at(), 0);
    caret.end(&text);
    caret.right(&text);
    assert_eq!(caret.at(), text.chars().count());
    let _ = &text;

    // Characters, not bytes: a title with an accent in it steps one
    // letter at a time and is never cut in half.
    let mut text = "Ishido".to_string();
    let mut caret = Caret::end_of(&text);
    caret.left();
    caret.insert(&mut text, 'ó');
    assert_eq!(text, "Ishidóo");
    assert_eq!(caret.at(), 6);
    assert!(caret.backspace(&mut text));
    assert_eq!(text, "Ishido");
}

#[cfg(feature = "game-library")]
#[test]
fn the_sign_in_caret_steps_through_the_mask() {
    let mut login = LoginDialog::default();
    for c in "hobbo".chars() {
        login.insert(c);
    }
    assert_eq!(login.user, "hobbo");
    assert_eq!(login.caret.at(), 5);

    // Correcting the middle of a name, without retyping the end.
    login.caret_move(CaretMove::Left);
    login.insert('9');
    login.insert('1');
    assert_eq!(login.user, "hobb91o");
    login.delete();
    assert_eq!(login.user, "hobb91");

    // The password is edited the same way, through the mask: the caret
    // counts characters, and the text itself never comes back out.
    login.focus_on(LoginField::Pass);
    assert_eq!(login.caret.at(), 0, "an empty box starts at the front");
    for c in "sekrit".chars() {
        login.insert(c);
    }
    assert_eq!(login.pass.chars(), 6);
    login.caret_move(CaretMove::Home);
    login.insert('X');
    assert_eq!(login.pass.expose(), "Xsekrit");
    assert_eq!(login.caret.at(), 1);
    login.backspace();
    assert_eq!(login.pass.expose(), "sekrit");
    assert_eq!(login.caret.at(), 0);
    // Backspace at the front does nothing, rather than eating forwards.
    login.backspace();
    assert_eq!(login.pass.expose(), "sekrit");
    login.delete();
    assert_eq!(login.pass.expose(), "ekrit");

    // Moving between boxes puts the caret at the end of the one moved
    // to, not wherever it was in the one left behind.
    login.caret_move(CaretMove::End);
    login.focus_on(LoginField::User);
    assert_eq!(login.caret.at(), login.user.chars().count());
}

#[cfg(feature = "game-library")]
#[test]
fn the_metadata_caret_amends_a_field_in_place() {
    let mut meta = MetaDialog {
        file: "Turrican.lha".to_string(),
        ..Default::default()
    };
    *meta.value_mut(MetaField::Name) = "Turican".to_string();
    *meta.value_mut(MetaField::Year) = "1990".to_string();
    meta.focus_on(MetaField::Name);
    assert_eq!(meta.caret.at(), 7);

    // The missing letter goes in where it belongs.
    for _ in 0..4 {
        meta.caret_move(CaretMove::Left);
    }
    meta.insert('r', 64);
    assert_eq!(meta.value(MetaField::Name), "Turrican");

    // A full box takes nothing more, wherever the caret is.
    meta.caret_move(CaretMove::Home);
    meta.insert('X', 8);
    assert_eq!(meta.value(MetaField::Name), "Turrican");

    // Moving on lands the caret at the end of the next box, and never
    // past the end of a shorter one.
    meta.focus_on(MetaField::Year);
    assert_eq!(meta.caret.at(), 4);
}

#[cfg(feature = "game-library")]
#[test]
fn the_favourites_list_scrolls_and_stays_inside_itself() {
    let mut state = LauncherState::new(MachineSetup::default());
    for at in 0..10 {
        state
            .library
            .db
            .toggle_favourite(&format!("Game{at}.lha"), &format!("Game {at}"));
    }
    assert_eq!(state.library.db.favourite_count(), 10);

    // Four rows on screen: the scroll stops with the last four in it
    // rather than running off the end into blank rows.
    state.scroll_favourites(3, 4);
    assert_eq!(state.library.favourite_scroll, 3);
    state.scroll_favourites(100, 4);
    assert_eq!(state.library.favourite_scroll, 6);
    state.scroll_favourites(-100, 4);
    assert_eq!(state.library.favourite_scroll, 0);

    // Walking the list with the keyboard drags the window after it.
    state.library.focus = LibraryFocus::Favourites;
    for _ in 0..9 {
        state.step_library_focus(1, 4);
    }
    assert_eq!(state.library.favourite_selected, 9);
    assert_eq!(state.library.favourite_scroll, 6, "the last row is drawn");
    state.step_library_focus(-9, 4);
    assert_eq!(state.library.favourite_selected, 0);
    assert_eq!(state.library.favourite_scroll, 0);

    // Removing from the end takes the selection and the window with it,
    // so neither is left pointing past a list that just got shorter.
    state.library.favourite_selected = 9;
    state.library.favourite_scroll = 6;
    state.remove_favourite(9);
    assert_eq!(state.library.db.favourite_count(), 9);
    assert_eq!(state.library.favourite_selected, 8);
    assert_eq!(state.library.favourite_scroll, 6);
}

#[cfg(feature = "game-library")]
#[test]
fn the_whdload_entry_sits_between_zorro_and_av() {
    // Where it is, and that turning it off takes out that one entry
    // and leaves every other in place.
    let with: Vec<LauncherTab> = tabs(true).to_vec();
    let without: Vec<LauncherTab> = tabs(false).to_vec();

    let at = with
        .iter()
        .position(|&t| t == LauncherTab::WhdloadLibrary)
        .expect("the strip carries WHDLoad");
    assert_eq!(with[at - 1], LauncherTab::Zorro);
    assert_eq!(with[at + 1], LauncherTab::AvAudio);

    // Off, it is gone -- and nothing else moved relative to itself.
    assert!(!without.contains(&LauncherTab::WhdloadLibrary));
    let mut minus = with.clone();
    minus.remove(at);
    assert_eq!(minus, without, "turning it off moved something else");
}

#[test]
#[cfg(feature = "game-library")]
fn whdload_is_its_own_strip_entry_and_opens_on_the_library() {
    // It left Storage: the nav row there no longer offers it, and
    // neither page returns to it.
    assert!(!LauncherTab::Storage
        .nav_options()
        .iter()
        .any(|&(_, tab)| tab == LauncherTab::Whdload));
    // Both pages light the one entry, which is the Library -- what the
    // strip opens on, and what the nav row lists first.
    assert_eq!(
        LauncherTab::Whdload.strip_tab(),
        LauncherTab::WhdloadLibrary
    );
    assert_eq!(
        LauncherTab::WhdloadLibrary.strip_tab(),
        LauncherTab::WhdloadLibrary
    );
    assert_eq!(LauncherTab::Whdload.parent_tab(), None);
    assert_eq!(LauncherTab::WhdloadLibrary.parent_tab(), None);
    assert_eq!(
        LauncherTab::WhdloadLibrary.nav_options().first(),
        Some(&("Library", LauncherTab::WhdloadLibrary))
    );
    // And the entry is in the strip when WHDLoad is on, and gone when
    // it is off.
    assert!(tabs(true).contains(&LauncherTab::WhdloadLibrary));
    assert!(!tabs(false).contains(&LauncherTab::WhdloadLibrary));
    // The row table: the game to launch, then what staging draws on.
    // The last two belong to the library, so they are only here in a
    // build that has one.
    let rows = rows(
        LauncherTab::Whdload,
        ParallelDevice::None,
        SerialMode::Off,
        false,
        false,
    );
    let labels: Vec<&str> = rows.iter().map(|r| r.label).collect();
    // What to boot and how first, then the places things live.
    let mut want = vec!["WHDLoad Settings:", "Launch game", "Machine type"];
    if cfg!(feature = "game-library") {
        want.push("OpenRetro");
    }
    want.extend([
        "Directories:",
        "WHDLoad package",
        "SKick package",
        "Kickstart ROMs",
    ]);
    if cfg!(feature = "game-library") {
        want.push("Game library");
    }
    want.push("Save data");
    assert_eq!(labels, want);
    // Every path row is a Drive row, so the whole host path shows.
    assert!(rows
        .iter()
        .filter(|r| r.field.is_whdload_path_field())
        .all(|r| r.kind == RowKind::Drive));
    // Directories browse as folders; the archives browse as files.
    assert!(!F::WhdloadGame.is_whdload_dir_field());
    assert!(F::WhdloadGame.is_whdload_archive_field());
    assert!(F::WhdloadWhdPackage.is_whdload_archive_field());
    assert!(F::WhdloadSkickPackage.is_whdload_archive_field());
    assert!(F::WhdloadKickstarts.is_whdload_dir_field());
    assert!(F::WhdloadLibrary.is_whdload_dir_field());
    // The game library is a folder of packages, so it browses as one.
    // Picking it with a file chooser is how it stayed unset.
    #[cfg(feature = "game-library")]
    assert!(F::WhdloadGames.is_whdload_dir_field());
}

#[test]
fn an_invalid_drive_name_is_reported_and_keeps_the_field_focused() {
    let mut state = LauncherState::from_raw(&RawConfig {
        filesys: vec![raw_mount("/host0")],
        ..RawConfig::default()
    });
    state.begin_edit_drive_name(LauncherField::Filesys0Dir);
    state.edit_buffer.clear();
    for c in "Work:1".chars() {
        state.edit_push(c);
    }
    state.edit_commit();
    let status = state.status.as_ref().expect("invalid name is reported");
    assert_eq!(status.kind, StatusKind::Error);
    assert!(status.text.contains("invalid character"), "{}", status.text);
    assert_eq!(state.editing(), Some(EditTarget::DriveName(F::Filesys0Dir)));

    // Fixing the name commits it.
    state.edit_backspace();
    state.edit_backspace();
    state.edit_commit();
    assert!(state.editing().is_none());
    assert_eq!(state.setup.drive_name(F::Filesys0Dir), Some("Work"));
}

#[test]
fn default_setup_is_the_a500_aros_machine() {
    let s = MachineSetup::default();
    assert_eq!(s.model, None);
    // With no profile chosen the picker highlights the A500 (the default
    // machine is the A500 defaults).
    assert_eq!(s.selected_model(), MachineModel::A500);
    assert_eq!(s.chipset, Chipset::Ecs);
    assert_eq!(s.cpu, CpuModel::M68000);
    assert_eq!(s.chip_ram, 512 * 1024);
    assert_eq!(s.slow_ram, 512 * 1024);
    assert!(s.rom.is_none(), "boot ROM defaults to bundled AROS");
    // The base A500 had no battery-backed clock.
    assert!(!s.toggle_value(LauncherField::Rtc));
    // The greyed Zorro III RAM explains why on this 24-bit machine.
    assert_eq!(
        s.disabled_reason(LauncherField::Z3Ram),
        Some("needs 32-bit CPU")
    );
    // A bare default emits no overrides at all.
    let toml = s.to_toml().unwrap();
    assert!(toml.trim().is_empty(), "expected empty TOML, got:\n{toml}");
    assert!(s.build_config().is_ok());
}

#[test]
fn launcher_cycles_to_the_68060_with_50mhz_defaults() {
    let mut s = MachineSetup::default();
    for _ in 0..CPUS.len() {
        if s.cpu == CpuModel::M68060 {
            break;
        }
        s.cycle(LauncherField::Cpu, true);
    }
    assert_eq!(s.cpu, CpuModel::M68060, "cycled to the 68060");
    assert_eq!(s.clock_mhz, 50.0, "50 MHz default");
    assert!(s.fpu, "on-die FPU defaults on");
    assert_eq!(s.disabled_reason(LauncherField::Icache), None);
    assert_eq!(s.disabled_reason(LauncherField::Dcache), None);
    assert!(s.toggle_value(LauncherField::Icache));
    assert!(s.toggle_value(LauncherField::Dcache));
}

#[test]
fn launcher_exposes_both_cache_toggles_for_the_68040() {
    let mut s = MachineSetup::default();
    // Step the CPU selector along to the 68040.
    for _ in 0..CPUS.len() {
        if s.cpu == CpuModel::M68040 {
            break;
        }
        s.cycle(LauncherField::Cpu, true);
    }
    assert_eq!(s.cpu, CpuModel::M68040, "cycled to the 68040");
    // The 040 has both caches, so neither toggle is greyed and both default
    // on (like the 030) when the part is selected.
    assert_eq!(s.disabled_reason(LauncherField::Icache), None);
    assert_eq!(s.disabled_reason(LauncherField::Dcache), None);
    assert!(s.toggle_value(LauncherField::Icache));
    assert!(s.toggle_value(LauncherField::Dcache));

    // The 68000 has neither; the 68EC020 has only the instruction cache.
    s.cpu = CpuModel::M68000;
    assert!(s.disabled_reason(LauncherField::Icache).is_some());
    assert!(s.disabled_reason(LauncherField::Dcache).is_some());
    s.cpu = CpuModel::M68EC020;
    assert_eq!(s.disabled_reason(LauncherField::Icache), None);
    assert!(s.disabled_reason(LauncherField::Dcache).is_some());
}

#[test]
fn select_model_applies_profile_defaults_and_emits_only_the_profile() {
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A1200));
    assert_eq!(s.chipset, Chipset::Aga);
    assert_eq!(s.cpu, CpuModel::M68EC020);
    assert_eq!(s.chip_ram, 2 * 1024 * 1024);
    // The base A1200 shipped without a populated RTC; the A500+ has one.
    assert!(!s.toggle_value(LauncherField::Rtc));
    s.select_model(Some(MachineModel::A500Plus));
    assert!(s.toggle_value(LauncherField::Rtc));
    s.select_model(Some(MachineModel::A1200));
    let raw = s.to_raw();
    assert_eq!(raw.machine.profile.as_deref(), Some("A1200"));
    // Everything else matches the profile default, so nothing else is set.
    assert!(raw.memory.chip.is_none());
    assert!(raw.cpu.model.is_none());
    assert!(raw.chipset.revision.is_none());
    assert!(s.build_config().is_ok());
}

#[test]
fn cd_profiles_select_zero_drives_and_the_launcher_can_override_them() {
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::Cd32));
    assert_eq!(s.floppy_drives, 0);
    assert_eq!(s.to_raw().floppy.drives, None);
    assert_eq!(s.build_config().unwrap().floppy_connected, [false; 4]);

    s.cycle(F::FloppyDrives, true);
    assert_eq!(s.floppy_drives, 1);
    assert_eq!(s.to_raw().floppy.drives, Some(1));

    s.select_model(Some(MachineModel::Cdtv));
    assert_eq!(s.floppy_drives, 0);
    s.select_model(Some(MachineModel::A500));
    assert_eq!(s.floppy_drives, 1);
}

#[test]
fn zero_floppy_drives_round_trips_as_an_a500_override() {
    let raw: RawConfig = toml::from_str("[floppy]\ndrives = 0").unwrap();
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.floppy_drives, 0);
    assert_eq!(setup.to_raw().floppy.drives, Some(0));
    assert_eq!(setup.build_config().unwrap().floppy_connected, [false; 4]);
}

#[test]
fn sparse_floppy_media_keeps_the_highest_bay_visible() {
    let path = std::env::temp_dir().join(format!(
        "copperline-launcher-sparse-{}-{}.adf",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, vec![0u8; crate::floppy::ADF_SIZE]).unwrap();

    let mut raw = RawConfig::default();
    raw.machine.profile = Some("CD32".to_string());
    raw.floppy.df2 = Some(RawFloppyDrive {
        path: Some(path.to_string_lossy().into_owned()),
        ..RawFloppyDrive::default()
    });
    let setup = MachineSetup::from_raw(&raw).expect("sparse media config loads");

    assert_eq!(setup.floppy_drives, 3);
    assert!(!setup.row_hidden(F::Df2Image));
    assert_eq!(setup.to_raw().floppy.drives, Some(3));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn floppy_count_cannot_hide_an_image_bearing_bay() {
    let mut setup = MachineSetup::default();
    setup.set_path(F::Df1Image, PathBuf::from("/disks/two.adf"));
    assert_eq!(setup.floppy_drives, 2);

    setup.cycle(F::FloppyDrives, false);
    assert_eq!(setup.floppy_drives, 2);
    assert!(!setup.row_hidden(F::Df1Image));
    assert_eq!(setup.to_raw().floppy.drives, Some(2));

    setup.clear_path(F::Df1Image);
    setup.cycle(F::FloppyDrives, false);
    assert_eq!(setup.floppy_drives, 1);
    assert!(setup.row_hidden(F::Df1Image));
}

#[test]
fn mouse_sensitivity_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    // The neutral midpoint shows as "Default" and matches the baseline, so
    // nothing is written.
    assert_eq!(s.value_label(LauncherField::MouseSensitivity), "Default");
    assert_eq!(s.to_raw().input.mouse_sensitivity, None);

    // Cycle down twice (step 1): 50 -> 49 -> 48, and it now persists.
    s.cycle(LauncherField::MouseSensitivity, false);
    s.cycle(LauncherField::MouseSensitivity, false);
    assert_eq!(s.value_label(LauncherField::MouseSensitivity), "48");
    assert_eq!(s.to_raw().input.mouse_sensitivity, Some(48));
}

#[test]
fn screen_tint_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    // Full colour is the baseline, so nothing is written for it.
    assert_eq!(s.value_label(LauncherField::Tint), "Colour");
    assert_eq!(s.to_raw().display.tint, None);

    s.cycle(LauncherField::Tint, true);
    assert_eq!(s.value_label(LauncherField::Tint), "Black & white");
    assert_eq!(s.to_raw().display.tint, Some("bw".to_string()));

    s.cycle(LauncherField::Tint, true);
    assert_eq!(s.value_label(LauncherField::Tint), "Green");
    assert_eq!(s.to_raw().display.tint, Some("green".to_string()));

    // The written config has to load back into the same setting.
    assert_eq!(s.build_config().expect("valid config").tint, Tint::Green);

    // Cycling backwards from the baseline wraps to the end of the list.
    let mut s = MachineSetup::default();
    s.cycle(LauncherField::Tint, false);
    assert_eq!(s.value_label(LauncherField::Tint), "Sepia");
    assert_eq!(s.to_raw().display.tint, Some("sepia".to_string()));
}

/// The module is offered as a source only while it is the destination,
/// and stops being one the moment the output moves elsewhere.
#[test]
#[cfg(all(feature = "midi", feature = "mt32"))]
fn the_mt32_is_a_midi_source_only_while_it_is_the_destination() {
    let mut s = MachineSetup {
        midi_out: Some(crate::config::MIDI_OUT_MT32.to_string()),
        ..MachineSetup::default()
    };
    assert!(s.midi_out_is_mt32());

    // With no host sources at all, the module is still there to pick.
    s.cycle(LauncherField::MidiIn, true);
    assert_eq!(
        s.value_label(LauncherField::MidiIn),
        crate::midi::MIDI_OUT_MT32_LABEL
    );

    // Moving the output off the module takes the input with it:
    // nothing reaches it, so it has nothing left to answer. The module
    // rides at the end of the output list, so one step wraps to None.
    s.cycle(LauncherField::MidiOut, true);
    assert!(!s.midi_out_is_mt32());
    assert_eq!(s.value_label(LauncherField::MidiIn), "None");

    // And it is no longer among the sources to cycle onto.
    s.cycle(LauncherField::MidiIn, true);
    assert_eq!(s.value_label(LauncherField::MidiIn), "None");
}

#[test]
fn the_mt32_rows_appear_only_when_it_is_the_midi_output() {
    let midi_rows = |out: Option<&str>| {
        let s = MachineSetup {
            midi_out: out.map(str::to_string),
            ..MachineSetup::default()
        };
        rows(
            LauncherTab::IoPorts,
            ParallelDevice::None,
            SerialMode::Midi,
            s.midi_out_is_mt32(),
            false,
        )
        .iter()
        .map(|r| r.field)
        .collect::<Vec<_>>()
    };

    // A host endpoint: the ROM pair and the panel are nothing to do with
    // it, so they are not offered.
    let host = midi_rows(Some("Some USB Interface"));
    assert!(host.contains(&LauncherField::MidiOut));
    assert!(!host.contains(&LauncherField::Mt32ControlRom));
    assert!(!host.contains(&LauncherField::Mt32Panel));

    // The built-in synth: both ROMs and the panel.
    let mt32 = midi_rows(Some(crate::config::MIDI_OUT_MT32));
    assert!(mt32.contains(&LauncherField::Mt32ControlRom));
    assert!(mt32.contains(&LauncherField::Mt32PcmRom));
    assert!(mt32.contains(&LauncherField::Mt32Panel));
}

#[test]
fn the_mt32_rom_pair_and_panel_round_trip_through_raw() {
    let mut s = MachineSetup::default();
    assert_eq!(s.to_raw().serial.mt32_control_rom, None);

    s.set_path(
        LauncherField::Mt32ControlRom,
        std::path::PathBuf::from("MT32_CONTROL.ROM"),
    );
    s.set_path(
        LauncherField::Mt32PcmRom,
        std::path::PathBuf::from("MT32_PCM.ROM"),
    );
    // The panel row cycles its two states now, arrows rather than
    // a checkbox.
    s.cycle(LauncherField::Mt32Panel, true);
    assert!(s.toggle_value(LauncherField::Mt32Panel));
    assert_eq!(s.value_label(LauncherField::Mt32Panel), "Enabled");
    s.cycle(LauncherField::Mt32Panel, false);
    assert_eq!(s.value_label(LauncherField::Mt32Panel), "Disabled");
    s.cycle(LauncherField::Mt32Panel, true);

    let raw = s.to_raw();
    assert_eq!(
        raw.serial.mt32_control_rom.as_deref(),
        Some("MT32_CONTROL.ROM")
    );
    assert_eq!(raw.serial.mt32_pcm_rom.as_deref(), Some("MT32_PCM.ROM"));
    assert_eq!(raw.serial.mt32_panel, Some(true));

    let reloaded = MachineSetup::from_raw(&raw).expect("valid raw");
    assert_eq!(
        reloaded.path(LauncherField::Mt32PcmRom),
        Some(std::path::Path::new("MT32_PCM.ROM"))
    );
    assert!(reloaded.toggle_value(LauncherField::Mt32Panel));
}

#[test]
fn menu_scale_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    // 1x is the baseline, so nothing is written for it. The launcher has
    // the width to name the size as well as give the figure.
    assert_eq!(s.value_label(LauncherField::MenuScale), "Normal (1x)");
    assert_eq!(s.to_raw().display.menu_scale, None);

    s.cycle(LauncherField::MenuScale, true);
    assert_eq!(s.value_label(LauncherField::MenuScale), "Large (2x)");
    assert_eq!(s.to_raw().display.menu_scale, Some("2x".to_string()));

    // The written config has to load back into the same setting.
    assert_eq!(
        s.build_config().expect("valid config").menu_scale,
        MenuScale::Large
    );
    let reloaded = MachineSetup::from_raw(&s.to_raw()).expect("valid raw");
    assert_eq!(reloaded.value_label(LauncherField::MenuScale), "Large (2x)");
}

#[test]
fn every_bezel_style_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    // Off is the baseline, so nothing is written for it.
    assert_eq!(
        s.value_label(LauncherField::Bezel),
        BezelStyle::None.menu_label()
    );
    assert_eq!(s.to_raw().display.bezel, None);

    // Cycling reaches every style, and each is written, reloaded and
    // shown back as itself.
    for _ in 0..BezelStyle::MENU_ORDER.len() {
        s.cycle(LauncherField::Bezel, true);
        let style = s.bezel;
        assert_eq!(
            s.build_config().expect("valid config").bezel,
            style,
            "{} did not survive the config",
            style.label()
        );
        let reloaded = MachineSetup::from_raw(&s.to_raw()).expect("valid raw");
        assert_eq!(
            reloaded.value_label(LauncherField::Bezel),
            style.menu_label()
        );
    }
    // A full turn of the cycle comes back to where it started.
    assert_eq!(s.bezel, BezelStyle::None);
}

#[test]
fn the_sticker_folder_survives_a_launcher_round_trip() {
    let mut raw = RawConfig::default();
    raw.display.bezel_stickers = Some("/data/amiga/stickers".into());
    let s = MachineSetup::from_raw(&raw).expect("valid raw");
    // No launcher row edits the folder, so a straight round-trip must
    // carry it: a machine saved or started from the launcher keeps its
    // stickers.
    assert_eq!(
        s.to_raw().display.bezel_stickers.as_deref(),
        Some("/data/amiga/stickers")
    );
    assert_eq!(
        s.build_config()
            .expect("valid config")
            .bezel_stickers
            .as_deref(),
        Some(Path::new("/data/amiga/stickers"))
    );
    // Unset stays unwritten.
    assert_eq!(
        MachineSetup::default().to_raw().display.bezel_stickers,
        None
    );
}

#[test]
fn deinterlace_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    // On is the baseline, so nothing is written for it.
    assert!(s.toggle_value(LauncherField::Deinterlace));
    assert_eq!(s.to_raw().display.deinterlace, None);

    s.cycle(LauncherField::Deinterlace, true);
    assert!(!s.toggle_value(LauncherField::Deinterlace));
    assert_eq!(s.to_raw().display.deinterlace, Some(false));

    // The written config has to load back into the same setting.
    assert!(!s.build_config().expect("valid config").deinterlace);
    let reloaded = MachineSetup::from_raw(&s.to_raw()).expect("valid raw");
    assert!(!reloaded.toggle_value(LauncherField::Deinterlace));
}

#[test]
fn mouse_capture_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    // Click-to-capture is the baseline, so nothing is written for it.
    assert_eq!(s.value_label(LauncherField::MouseCapture), "On click");
    assert_eq!(s.to_raw().input.mouse_capture, None);

    s.cycle(LauncherField::MouseCapture, true);
    assert_eq!(s.value_label(LauncherField::MouseCapture), "Automatic");
    assert_eq!(s.to_raw().input.mouse_capture, Some("auto".to_string()));

    s.cycle(LauncherField::MouseCapture, true);
    assert_eq!(s.value_label(LauncherField::MouseCapture), "Shortcut only");
    assert_eq!(s.to_raw().input.mouse_capture, Some("manual".to_string()));

    // The written config has to load back into the same setting.
    assert_eq!(
        s.build_config().expect("valid config").mouse_capture,
        MouseCapture::Manual
    );
}

#[test]
fn mouse_capture_greys_out_without_a_mouse() {
    let mut s = MachineSetup::default();
    assert_eq!(s.disabled_reason(LauncherField::MouseCapture), None);
    s.port_devices = [PortDevice::Joystick, PortDevice::Joystick];
    assert_eq!(
        s.disabled_reason(LauncherField::MouseCapture),
        Some("No mouse")
    );
}

#[test]
fn mouse_sensitivity_greys_out_without_a_mouse() {
    let mut s = MachineSetup::default();
    // The default A500 has a mouse in port 1, so it is active.
    assert_eq!(s.disabled_reason(LauncherField::MouseSensitivity), None);

    // Neither port a mouse: greyed.
    s.port_devices = [PortDevice::Joystick, PortDevice::Joystick];
    assert_eq!(
        s.disabled_reason(LauncherField::MouseSensitivity),
        Some("No mouse")
    );

    // A mouse in either port re-enables it.
    s.port_devices = [PortDevice::Joystick, PortDevice::Mouse];
    assert_eq!(s.disabled_reason(LauncherField::MouseSensitivity), None);
}

#[test]
fn rtg_card_round_trips_through_raw() {
    // An A4000 hosts Zorro III, so it comes with the card fitted; that
    // matches its baseline, so nothing is written for it.
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A4000));
    assert_eq!(s.rtg, RtgCard::Z3660);
    assert_eq!(s.value_label(LauncherField::Rtg), "Z3660");
    assert!(s.to_raw().rtg.card.is_none());

    // Turning it off differs from the baseline, so it is written, and
    // the written key is what [rtg] card parses back rather than the
    // display label -- the parse is case-forgiving, the round trip
    // should not lean on that.
    s.cycle(LauncherField::Rtg, true);
    assert_eq!(s.rtg, RtgCard::None);
    assert_eq!(s.value_label(LauncherField::Rtg), "None");
    let raw = s.to_raw();
    assert_eq!(raw.rtg.card.as_deref(), Some("none"));
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.rtg, RtgCard::None);
    assert!(s.build_config().is_ok());

    // A 68000 machine cannot host Z3660, but its Zorro II bus can host a
    // Picasso II family. The selector therefore remains live and round-trips the
    // parser spelling rather than its friendlier display name.
    s.select_model(Some(MachineModel::A500));
    assert_eq!(s.rtg, RtgCard::None);
    assert_eq!(s.disabled_reason(LauncherField::Rtg), None);
    s.cycle(LauncherField::Rtg, true);
    assert_eq!(s.rtg, RtgCard::Picasso2);
    assert_eq!(s.value_label(LauncherField::Rtg), "Picasso II");
    let raw = s.to_raw();
    assert_eq!(raw.rtg.card.as_deref(), Some("picasso2"));
    assert_eq!(MachineSetup::from_raw(&raw).unwrap().rtg, RtgCard::Picasso2);
    assert!(s.build_config().is_ok());

    s.cycle(LauncherField::Rtg, true);
    assert_eq!(s.rtg, RtgCard::Picasso2Plus);
    assert_eq!(s.value_label(LauncherField::Rtg), "Picasso II+");
    let raw = s.to_raw();
    assert_eq!(raw.rtg.card.as_deref(), Some("picasso2plus"));
    assert_eq!(
        MachineSetup::from_raw(&raw).unwrap().rtg,
        RtgCard::Picasso2Plus
    );
    assert!(s.build_config().is_ok());

    // Graffity [Zorro II] is a Zorro II card too, so it cycles on a
    // 68000 machine right after the Picasso II family; the Zorro III
    // cards do not (the cycle wraps back to None instead).
    s.cycle(LauncherField::Rtg, true);
    assert_eq!(s.rtg, RtgCard::GraffityZ2);
    assert_eq!(s.value_label(LauncherField::Rtg), "Graffity Z2");
    let raw = s.to_raw();
    assert_eq!(raw.rtg.card.as_deref(), Some("graffityz2"));
    assert_eq!(
        MachineSetup::from_raw(&raw).unwrap().rtg,
        RtgCard::GraffityZ2
    );
    assert!(s.build_config().is_ok());
    s.cycle(LauncherField::Rtg, true);
    assert_eq!(s.rtg, RtgCard::None);

    // A loaded 1 MB board preserves its fitted VRAM when saved.
    let mut raw = RawConfig::default();
    raw.rtg.card = Some("picasso2".to_string());
    raw.rtg.vram = Some("1M".to_string());
    let s = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(s.rtg_vram_bytes, 1024 * 1024);
    assert_eq!(s.to_raw().rtg.vram.as_deref(), Some("1M"));
}

/// The Video page's Scaling picker writes the `[display] scaling` key
/// its parser reads back, and stays out of the file while it matches
/// the default.
#[test]
fn display_scaling_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    assert_eq!(s.scaling, DisplayScaling::Smooth);
    assert_eq!(s.value_label(LauncherField::Scaling), "Smooth");
    assert!(s.to_raw().display.scaling.is_none());

    s.cycle(LauncherField::Scaling, true);
    assert_eq!(s.scaling, DisplayScaling::Integer);
    assert_eq!(s.value_label(LauncherField::Scaling), "Integer");
    let raw = s.to_raw();
    assert_eq!(raw.display.scaling.as_deref(), Some("integer"));
    assert_eq!(
        MachineSetup::from_raw(&raw).unwrap().scaling,
        DisplayScaling::Integer
    );
    assert!(s.build_config().is_ok());

    // Two modes, so cycling on returns to the default.
    s.cycle(LauncherField::Scaling, true);
    assert_eq!(s.scaling, DisplayScaling::Smooth);
}

#[test]
fn cycling_chip_ram_walks_the_presets() {
    let mut s = MachineSetup::default();
    assert_eq!(s.chip_ram, 512 * 1024);
    s.cycle(LauncherField::ChipRam, true);
    assert_eq!(s.chip_ram, 1024 * 1024);
    s.cycle(LauncherField::ChipRam, true);
    assert_eq!(s.chip_ram, 2 * 1024 * 1024);
    s.cycle(LauncherField::ChipRam, false);
    assert_eq!(s.chip_ram, 1024 * 1024);
}

#[test]
fn agnus_override_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    s.cycle(LauncherField::Agnus, true); // None -> Some(OCS)
    assert_eq!(s.agnus, Some(AgnusRevision::Ocs));
    let raw = s.to_raw();
    assert_eq!(raw.chipset.agnus.as_deref(), Some("OCS"));
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.agnus, Some(AgnusRevision::Ocs));
}

#[test]
fn serial_tcp_listen_round_trips_through_raw() {
    // A launcher save must not drop the [serial] listen override, whether
    // or not the tab that edits it was ever opened (regression: it was
    // absent from MachineSetup/to_raw altogether).
    let mut raw = RawConfig::default();
    raw.serial.mode = Some("tcp".into());
    raw.serial.listen = Some("0.0.0.0:2323".into());
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.serial_listen.as_deref(), Some("0.0.0.0:2323"));
    let back = setup.to_raw();
    assert_eq!(back.serial.listen.as_deref(), Some("0.0.0.0:2323"));
}

#[cfg(feature = "midi")]
#[test]
fn serial_mode_cycle_always_offers_tcp_connect() {
    // tcp-connect used to be skipped unless the loaded config already
    // carried a dial-out address, because nothing here could type one.
    // The Connect box can, so every mode is now reachable from the
    // picker whatever the config arrived holding.
    let mut setup = MachineSetup {
        serial_mode: SerialMode::Tcp,
        ..Default::default()
    };
    setup.cycle(LauncherField::SerialMode, true);
    assert_eq!(setup.serial_mode, SerialMode::TcpConnect);

    // And the picker walks the whole list either way round.
    let mut seen: Vec<SerialMode> = Vec::new();
    for _ in 0..SERIAL_MODES.len() * 2 {
        if !seen.contains(&setup.serial_mode) {
            seen.push(setup.serial_mode);
        }
        setup.cycle(LauncherField::SerialMode, false);
    }
    assert_eq!(seen.len(), SERIAL_MODES.len());
}

#[cfg(feature = "midi")]
#[test]
fn serial_address_rows_appear_only_in_their_tcp_mode() {
    let has = |mode, field| {
        rows(
            LauncherTab::IoPorts,
            ParallelDevice::None,
            mode,
            false,
            false,
        )
        .iter()
        .any(|r| r.field == field)
    };
    // Dialling out needs somewhere to dial; listening needs somewhere to
    // bind. Neither mode carries the other's address, and the modes with
    // no address at all show neither box.
    assert!(has(SerialMode::TcpConnect, LauncherField::SerialConnect));
    assert!(!has(SerialMode::TcpConnect, LauncherField::SerialListen));
    assert!(has(SerialMode::Tcp, LauncherField::SerialListen));
    assert!(!has(SerialMode::Tcp, LauncherField::SerialConnect));
    for mode in [SerialMode::Off, SerialMode::Stdout, SerialMode::Midi] {
        assert!(!has(mode, LauncherField::SerialConnect));
        assert!(!has(mode, LauncherField::SerialListen));
    }
    // Both boxes are free-text rows, so the panel draws and hit-tests
    // them with the value-box widget.
    for (mode, field) in [
        (SerialMode::TcpConnect, LauncherField::SerialConnect),
        (SerialMode::Tcp, LauncherField::SerialListen),
    ] {
        let r = rows(
            LauncherTab::IoPorts,
            ParallelDevice::None,
            mode,
            false,
            false,
        );
        let found = r.iter().find(|r| r.field == field).unwrap();
        assert_eq!(found.kind, RowKind::Text);
        assert!(LauncherState::is_serial_addr(field));
    }
}

#[cfg(feature = "midi")]
#[test]
fn modem_mode_rows_are_listen_and_telnet_only() {
    let has = |field| {
        rows(
            LauncherTab::IoPorts,
            ParallelDevice::None,
            SerialMode::Modem,
            false,
            false,
        )
        .iter()
        .any(|r| r.field == field)
    };
    // The modem shares the Listen box with plain TCP-listen mode (where
    // incoming calls come from) and adds its own Telnet default toggle;
    // it has nothing to dial itself (that's ATD's job), so no Connect box.
    assert!(has(LauncherField::SerialListen));
    assert!(has(LauncherField::SerialTelnet));
    assert!(!has(LauncherField::SerialConnect));
    // And the toggle only shows for the modem: the other TCP mode and
    // the address-free modes have no AT*T default to edit.
    for mode in [
        SerialMode::Off,
        SerialMode::Stdout,
        SerialMode::Midi,
        SerialMode::Tcp,
        SerialMode::TcpConnect,
    ] {
        assert!(!rows(
            LauncherTab::IoPorts,
            ParallelDevice::None,
            mode,
            false,
            false
        )
        .iter()
        .any(|r| r.field == LauncherField::SerialTelnet));
    }
}

#[cfg(feature = "midi")]
#[test]
fn serial_telnet_toggle_round_trips_through_raw() {
    let mut s = MachineSetup {
        serial_mode: SerialMode::Modem,
        ..Default::default()
    };
    assert!(!s.toggle_value(LauncherField::SerialTelnet));
    assert_eq!(s.value_label(LauncherField::SerialTelnet), "Disabled");
    s.cycle(LauncherField::SerialTelnet, true);
    assert!(s.toggle_value(LauncherField::SerialTelnet));
    assert_eq!(s.value_label(LauncherField::SerialTelnet), "Enabled");

    let raw = s.to_raw();
    assert_eq!(raw.serial.mode.as_deref(), Some("modem"));
    assert_eq!(raw.serial.telnet, Some(true));

    let reloaded = MachineSetup::from_raw(&raw).expect("valid raw");
    assert!(reloaded.toggle_value(LauncherField::SerialTelnet));

    // Cycling back to the default must not leave a redundant override in
    // a saved config (same contract as every other bool row, e.g. the
    // MT-32 panel above).
    s.cycle(LauncherField::SerialTelnet, false);
    assert_eq!(s.to_raw().serial.telnet, None);
}

#[cfg(feature = "midi")]
#[test]
fn modem_listen_round_trips_through_raw() {
    // Incoming calls bind the same [serial] listen key TCP-listen mode
    // uses; the modem mode must carry it just as faithfully.
    let mut raw = RawConfig::default();
    raw.serial.mode = Some("modem".into());
    raw.serial.listen = Some("0.0.0.0:2323".into());
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.serial_mode, SerialMode::Modem);
    assert_eq!(setup.serial_listen.as_deref(), Some("0.0.0.0:2323"));
    let back = setup.to_raw();
    assert_eq!(back.serial.mode.as_deref(), Some("modem"));
    assert_eq!(back.serial.listen.as_deref(), Some("0.0.0.0:2323"));
}

#[cfg(feature = "midi")]
#[test]
fn typing_a_serial_connect_address_sets_it_and_round_trips() {
    let mut state = LauncherState::new(MachineSetup::default());
    state.setup.serial_mode = SerialMode::TcpConnect;
    state.begin_edit_serial_addr(LauncherField::SerialConnect, false);
    assert_eq!(state.edit_buffer(), "", "an unset box starts empty");
    for c in "bbs.example.com".chars() {
        state.edit_push(c);
    }
    state.edit_commit();
    assert!(state.editing().is_none());
    state.begin_edit_serial_addr(LauncherField::SerialConnect, true);
    for c in "1337".chars() {
        state.edit_push(c);
    }
    state.edit_commit();
    assert!(state.editing().is_none());
    assert_eq!(
        state.setup.serial_connect.as_deref(),
        Some("bbs.example.com:1337")
    );
    // What was typed is what a Save writes.
    let raw = state.setup.to_raw();
    assert_eq!(raw.serial.mode.as_deref(), Some("tcp-connect"));
    assert_eq!(raw.serial.connect.as_deref(), Some("bbs.example.com:1337"));

    // Re-opening a box starts from the half it holds, not from a
    // placeholder, so an edit is a correction rather than a retype.
    state.begin_edit_serial_addr(LauncherField::SerialConnect, false);
    assert_eq!(state.edit_buffer(), "bbs.example.com");
    state.edit_cancel();
    state.begin_edit_serial_addr(LauncherField::SerialConnect, true);
    assert_eq!(state.edit_buffer(), "1337");
    state.edit_cancel();
}

#[cfg(feature = "midi")]
#[test]
fn half_a_serial_address_completes_with_the_defaults() {
    let mut state = LauncherState::new(MachineSetup::default());
    state.setup.serial_mode = SerialMode::Tcp;
    // A host on its own takes the default port...
    state.begin_edit_serial_addr(LauncherField::SerialListen, false);
    for c in "0.0.0.0".chars() {
        state.edit_push(c);
    }
    state.edit_commit();
    // The session keeps only what was typed; the config a Save writes (and
    // a Run uses) gets the default port filled in.
    assert_eq!(state.setup.serial_listen.as_deref(), Some("0.0.0.0"));
    assert_eq!(
        state.setup.to_raw().serial.listen.as_deref(),
        Some(&*format!("0.0.0.0:{}", crate::config::SERIAL_DEFAULT_PORT))
    );
    // ...and emptying the host leaves the port on the default host.
    state.begin_edit_serial_addr(LauncherField::SerialListen, true);
    for c in "2323".chars() {
        state.edit_push(c);
    }
    state.edit_commit();
    state.begin_edit_serial_addr(LauncherField::SerialListen, false);
    for _ in 0..16 {
        state.edit_backspace();
    }
    state.edit_commit();
    assert_eq!(state.setup.serial_listen.as_deref(), Some(":2323"));
    assert_eq!(
        state.setup.to_raw().serial.listen.as_deref(),
        Some(&*format!("{}:2323", crate::config::SERIAL_DEFAULT_HOST))
    );
}

#[cfg(feature = "midi")]
#[test]
fn a_port_out_of_range_keeps_the_focus() {
    let mut state = LauncherState::new(MachineSetup::default());
    for bad in ["0", "65536", "99999"] {
        state.begin_edit_serial_addr(LauncherField::SerialListen, true);
        for c in bad.chars() {
            state.edit_push(c);
        }
        state.edit_commit();
        assert_eq!(
            state.editing(),
            Some(EditTarget::SerialPort(LauncherField::SerialListen)),
            "{bad} was accepted"
        );
        assert!(state.status.is_some(), "{bad} was refused silently");
        assert_eq!(state.setup.serial_listen, None);
        state.edit_cancel();
    }
    // Deleting the port is not an error: it reverts to the default.
    state.begin_edit_serial_addr(LauncherField::SerialListen, true);
    state.edit_commit();
    assert!(state.editing().is_none());
}

#[cfg(feature = "midi")]
#[test]
fn an_ipv6_host_keeps_its_brackets_in_the_stored_address() {
    let mut state = LauncherState::new(MachineSetup::default());
    // A bare IPv6 literal is full of colons, so the stored spelling wraps
    // it in brackets; typed with brackets they come off and go back on.
    for typed in ["::1", "[::1]"] {
        state.begin_edit_serial_addr(LauncherField::SerialConnect, false);
        for c in typed.chars() {
            state.edit_push(c);
        }
        state.edit_commit();
        assert!(state.editing().is_none());
        state.begin_edit_serial_addr(LauncherField::SerialConnect, true);
        for c in "1337".chars() {
            state.edit_push(c);
        }
        state.edit_commit();
        assert_eq!(state.setup.serial_connect.as_deref(), Some("[::1]:1337"));
        // The host box re-opens on the bare literal.
        state.begin_edit_serial_addr(LauncherField::SerialConnect, false);
        assert_eq!(state.edit_buffer(), "::1");
        state.edit_cancel();
        state.setup.serial_connect = None;
    }
}

#[cfg(feature = "midi")]
#[test]
fn emptying_a_serial_address_box_unsets_it() {
    let mut state = LauncherState::new(MachineSetup {
        serial_mode: SerialMode::Tcp,
        serial_listen: Some("0.0.0.0:2323".into()),
        ..Default::default()
    });
    // The box shows what is bound; emptying it returns to the default,
    // which is what the value column then shows.
    assert_eq!(
        state.setup.value_label(LauncherField::SerialListen),
        "0.0.0.0:2323"
    );
    state.begin_edit_serial_addr(LauncherField::SerialListen, false);
    for _ in 0..64 {
        state.edit_backspace();
    }
    state.edit_commit();
    state.begin_edit_serial_addr(LauncherField::SerialListen, true);
    for _ in 0..64 {
        state.edit_backspace();
    }
    state.edit_commit();
    assert_eq!(state.setup.serial_listen, None);
    assert_eq!(
        state.setup.value_label(LauncherField::SerialListen),
        crate::config::SERIAL_TCP_DEFAULT_LISTEN
    );
    assert!(state.setup.to_raw().serial.listen.is_none());
}

#[cfg(feature = "midi")]
#[test]
fn a_dial_out_port_needs_no_privilege() {
    // Telnet's 23 is the canonical BBS port; dialing out binds nothing,
    // so the low-port gate is the listen box's alone and the Connect box
    // takes it whoever runs the process.
    let mut state = LauncherState::new(MachineSetup::default());
    state.begin_edit_serial_addr(LauncherField::SerialConnect, true);
    for c in "23".chars() {
        state.edit_push(c);
    }
    state.edit_commit();
    assert!(state.editing().is_none(), "port 23 dial-out was refused");
    assert_eq!(state.setup.serial_connect.as_deref(), Some(":23"));
    // And no host is invented for it: the half-address reaches the config
    // as typed, and the run explains what is missing.
    assert_eq!(state.setup.to_raw().serial.connect.as_deref(), Some(":23"));
}

#[cfg(feature = "midi")]
#[test]
fn old_style_host_port_typed_into_the_host_box_finds_both_halves() {
    // The single box took host:port for years; fingers will keep typing
    // it. A trailing :digits on a colon-free head routes each half where
    // it belongs, while a bare IPv6 literal keeps its colons.
    let mut state = LauncherState::new(MachineSetup::default());
    state.begin_edit_serial_addr(LauncherField::SerialConnect, false);
    for c in "bbs.example.com:2323".chars() {
        state.edit_push(c);
    }
    state.edit_commit();
    assert!(state.editing().is_none());
    assert_eq!(
        state.setup.serial_connect.as_deref(),
        Some("bbs.example.com:2323")
    );
    state.begin_edit_serial_addr(LauncherField::SerialConnect, true);
    assert_eq!(state.edit_buffer(), "2323");
    state.edit_cancel();
}

#[cfg(feature = "midi")]
#[test]
fn an_address_the_launcher_did_not_write_passes_through_untouched() {
    // A hand-written config can hold what the boxes would never store; the
    // round trip must not quietly repair it into a bind the config never
    // named -- the run fails loudly instead, as it always did.
    for weird in ["localhost:telnet", "::1", "127.0.0.1:70000"] {
        let state = LauncherState::new(MachineSetup {
            serial_mode: SerialMode::Tcp,
            serial_listen: Some(weird.into()),
            ..Default::default()
        });
        assert_eq!(
            state.setup.to_raw().serial.listen.as_deref(),
            Some(weird),
            "{weird} was rewritten"
        );
    }
}

#[cfg(feature = "midi")]
#[test]
fn the_serial_boxes_take_only_what_their_half_can_hold() {
    let mut state = LauncherState::new(MachineSetup::default());
    // A space is not part of a host, and neither is a control code.
    state.begin_edit_serial_addr(LauncherField::SerialListen, false);
    for c in "127. 0\t.0.1\n".chars() {
        state.edit_push(c);
    }
    assert_eq!(state.edit_buffer(), "127.0.0.1");
    // And the box has an end: a stuck key cannot grow it without bound.
    for _ in 0..SERIAL_HOST_MAX * 2 {
        state.edit_push('9');
    }
    assert_eq!(state.edit_buffer().chars().count(), SERIAL_HOST_MAX);
    state.edit_cancel();
    // The port box is digits only, and no wider than a port.
    state.begin_edit_serial_addr(LauncherField::SerialListen, true);
    for c in "6x5-5.3 599".chars() {
        state.edit_push(c);
    }
    assert_eq!(state.edit_buffer(), "65535");
    state.edit_cancel();
}

#[test]
fn serial_tcp_connect_round_trips_through_raw() {
    // Same contract as the listen override: loading and saving carry the
    // dial-out address unchanged when nothing retypes it.
    let mut raw = RawConfig::default();
    raw.serial.mode = Some("tcp-connect".into());
    raw.serial.connect = Some("bbs.example.com:1337".into());
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.serial_mode, SerialMode::TcpConnect);
    assert_eq!(
        setup.serial_connect.as_deref(),
        Some("bbs.example.com:1337")
    );
    let back = setup.to_raw();
    assert_eq!(back.serial.mode.as_deref(), Some("tcp-connect"));
    assert_eq!(back.serial.connect.as_deref(), Some("bbs.example.com:1337"));
}

#[test]
fn parallel_output_round_trips_through_raw() {
    // The launcher has no printer-path editor, so loading and saving must
    // preserve a hand-written capture path (and its implied Printer device)
    // unchanged.
    let mut raw = RawConfig::default();
    raw.parallel.output = Some("captures/printer.raw".into());
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.parallel_device, ParallelDevice::Printer);
    assert_eq!(
        setup.parallel_output.as_deref(),
        Some(std::path::Path::new("captures/printer.raw"))
    );
    let back = setup.to_raw();
    assert_eq!(back.parallel.device.as_deref(), Some("printer"));
    assert_eq!(
        back.parallel.output.as_deref(),
        Some("captures/printer.raw")
    );
}

/// The workshop's pages carry every option the image formats have, and
/// none of them is a machine setting: nothing here may reach the
/// configuration a Save would write.
#[test]
fn the_disk_image_pages_edit_no_machine_setting() {
    let mut state = LauncherState::new(MachineSetup::default());
    let before = state.setup.to_raw();
    for tab in [
        LauncherTab::CreateFloppy,
        LauncherTab::CreateHard,
        LauncherTab::CreateGeometry,
    ] {
        for r in rows(
            tab,
            ParallelDevice::None,
            SerialMode::default(),
            false,
            false,
        )
        .iter()
        {
            // The page heading is inert and carries no field.
            if r.kind == RowKind::SectionHeader {
                continue;
            }
            assert!(
                LauncherState::is_workshop(r.field),
                "{:?} is not a workshop field",
                r.field
            );
            // Work every control the row offers, whichever kind it is:
            // a control added here later must be worked here too, or
            // this stops proving anything.
            state.workshop_cycle(r.field, true);
            state.workshop_cycle(r.field, false);
            state.workshop_toggle_flip(r.field);
            for family in FsFamily::ALL {
                state.workshop_set_fs_family(r.field, family);
            }
            for variant in crate::diskimage::Variant::ALL {
                state.workshop_set_fs_variant(r.field, variant);
            }
            // Typing into it, and pressing it if it is a button.
            state.begin_edit_new_image(r.field);
            for c in "XY9".chars() {
                state.edit_push(c);
            }
            state.edit_commit();
            state.edit_cancel();
        }
    }
    // And the two things the workshop can be asked to work out for
    // itself, which reach further than a single row.
    state.workshop.geometry_from_size();
    let _ = state.workshop.hard_spec();
    let _ = state.workshop.floppy_spec();

    assert_eq!(
        state.setup.to_raw(),
        before,
        "the workshop changed the machine configuration"
    );
    // The saved file is that same structure, so nothing here can reach
    // a TOML key by another route either.
    assert_eq!(
        state.setup.to_toml().unwrap(),
        before.to_toml_string().unwrap()
    );
}

/// Each picker reaches every value it offers and comes back, and the
/// value column always has something to say.
#[test]
fn the_disk_image_pickers_cover_their_choices() {
    use crate::diskimage::{Container, Density, FileSystem, Partitioning};
    let mut state = LauncherState::new(MachineSetup::default());

    let mut seen = std::collections::HashSet::new();
    for _ in 0..Density::ALL.len() * 2 {
        seen.insert(state.workshop.density);
        state.workshop_cycle(F::NewFloppyDensity, true);
    }
    assert_eq!(seen.len(), Density::ALL.len());

    let mut seen = std::collections::HashSet::new();
    for _ in 0..Container::ALL.len() * 2 {
        seen.insert(state.workshop.container);
        state.workshop_cycle(F::NewFloppyContainer, true);
    }
    assert_eq!(seen.len(), Container::ALL.len());

    // Unformatted, then all eight DOS tags, from the two tick rows.
    let mut seen = std::collections::HashSet::new();
    state.workshop_set_fs_family(F::NewFloppyFs, FsFamily::Unformatted);
    seen.insert(state.workshop.floppy_fs.map(|f| f.dos_type()));
    for family in [FsFamily::Ofs, FsFamily::Ffs] {
        for variant in crate::diskimage::Variant::ALL {
            state.workshop_set_fs_family(F::NewFloppyFs, family);
            set_dostype(&mut state, F::NewFloppyFs, variant);
            seen.insert(state.workshop.floppy_fs.map(|f| f.dos_type()));
        }
    }
    assert_eq!(seen.len(), 9, "unformatted plus DOS0..DOS7");
    assert!(seen.contains(&None));

    let mut seen = std::collections::HashSet::new();
    for _ in 0..Partitioning::ALL.len() * 2 {
        seen.insert(state.workshop.partitioning);
        state.workshop_cycle(F::NewHardPartitioning, true);
    }
    assert_eq!(seen.len(), Partitioning::ALL.len());

    // The size steppers stop at each end of what the box accepts, and
    // the unit swaps without touching the number.
    state.workshop.size = 1;
    state.workshop_cycle(F::NewHardSize, false);
    assert_eq!(state.workshop.size, 1, "size stepped below one");
    state.workshop.size = NEW_HARD_SIZE_MAX;
    state.workshop_cycle(F::NewHardSize, true);
    assert_eq!(
        state.workshop.size, NEW_HARD_SIZE_MAX,
        "size stepped past the max"
    );
    state.workshop.size = 8;
    state.workshop.size_unit = SizeUnit::Mb;
    assert_eq!(state.workshop.bytes(), 8 << 20);
    state.workshop.flip_size_unit();
    assert_eq!(
        state.workshop.size, 8,
        "flipping the unit changed the number"
    );
    assert_eq!(state.workshop.bytes(), 8 << 30);
    state.workshop.flip_size_unit();
    assert_eq!(state.workshop.size_unit, SizeUnit::Mb);

    // Every row shows something in its value column.
    for tab in [LauncherTab::CreateFloppy, LauncherTab::CreateHard] {
        for r in rows(
            tab,
            ParallelDevice::None,
            SerialMode::default(),
            false,
            false,
        )
        .iter()
        {
            match r.kind {
                RowKind::Cycle | RowKind::Text | RowKind::Size => assert!(
                    !state.row_value(r.field).is_empty(),
                    "{:?} shows nothing",
                    r.field
                ),
                RowKind::Action => assert!(
                    !state.workshop_action_label(r.field).is_empty(),
                    "{:?} has no button wording",
                    r.field
                ),
                _ => {}
            }
        }
    }
    let _ = FileSystem::OFS;
}

/// What the pages describe is what gets built, and the rows that mean
/// nothing for a given choice grey out.
#[test]
fn the_disk_image_pages_describe_what_gets_built() {
    use crate::diskimage::Partitioning;
    let mut state = LauncherState::new(MachineSetup::default());

    // Defaults: a plain OFS floppy, and an RDB hard drive with FFS.
    assert_eq!(
        state.workshop.floppy_spec().filesystem,
        Some(crate::diskimage::FileSystem::OFS)
    );
    let hard = state.workshop.hard_spec();
    assert_eq!(hard.partitioning, Partitioning::Rdb);
    assert_eq!(hard.device, "DH0");
    assert_eq!(hard.bytes, state.workshop.bytes());

    // An unformatted floppy has nothing to boot and nothing to name.
    state.workshop_set_fs_family(F::NewFloppyFs, FsFamily::Unformatted);
    assert!(!state.workshop_applies(F::NewFloppyBootable));
    assert!(!state.workshop_applies(F::NewFloppyLabel));
    // ...and asking for bootable anyway cannot produce boot code.
    state.workshop.floppy_bootable = true;
    assert!(!state.workshop.floppy_spec().bootable);

    // Without a partition table there is no entry to carry a device
    // name or a boot flag.
    state.workshop.partitioning = Partitioning::None;
    assert!(!state.workshop_applies(F::NewHardDevice));
    assert!(!state.workshop_applies(F::NewHardBootable));

    // Geometry follows the size until it is set by hand, and once it
    // is, the size can move without disturbing it.
    assert!(!state.workshop.geometry_custom);
    state.workshop.size = 100;
    let derived = state.workshop.effective_geometry();
    assert_eq!(
        derived,
        crate::diskimage::Geometry::for_size(state.workshop.bytes())
    );
    state.workshop.geometry_from_size();
    state.workshop.geometry_custom = true;
    state.workshop.size = 200;
    assert_eq!(state.workshop.effective_geometry(), derived);

    // The suggested file name follows the volume and the kind.
    state.workshop.floppy_label = "My Disk".into();
    assert_eq!(state.workshop.suggested_name(true), "MyDisk.adf");
    state.workshop.hard_label = String::new();
    assert_eq!(state.workshop.suggested_name(false), "image.hdf");
}

/// Drive the workshop pages the way a user does -- type into the boxes,
/// walk the pickers, flip the ticks -- and check every one of those
/// choices survives all the way into the bytes on disk. A setting that
/// looks right on the page and never reaches the image is the one
/// failure this feature cannot afford.
#[test]
fn every_workshop_setting_reaches_the_image() {
    use crate::diskimage::{Container, Density, FileSystem, Partitioning};

    fn scratch(name: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("copperline-workshop-{n}-{name}"))
    }
    fn long(data: &[u8], block: u64, long: usize) -> u32 {
        let at = block as usize * 512 + long * 4;
        u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
    }
    // Clicking a box seeds it with what is already there, so typing a
    // fresh value means clearing it first -- exactly what a user does.
    fn typed(state: &mut LauncherState, field: LauncherField, text: &str) {
        state.begin_edit_new_image(field);
        while !state.edit_buffer().is_empty() {
            state.edit_backspace();
        }
        for c in text.chars() {
            state.edit_push(c);
        }
        state.edit_commit();
        assert_eq!(state.editing(), None, "{field:?} refused \"{text}\"");
    }

    // --- the floppy page ---
    let mut state = LauncherState::new(MachineSetup::default());
    state.tab = LauncherTab::CreateFloppy;
    state.workshop_cycle(F::NewFloppyDensity, true); // DD -> HD
    state.workshop_cycle(F::NewFloppyContainer, true); // ADF -> extended
    state.workshop_set_fs_family(F::NewFloppyFs, FsFamily::Ffs);
    typed(&mut state, F::NewFloppyLabel, "Scratch");
    state.workshop_toggle_flip(F::NewFloppyBootable);
    let spec = state.workshop.floppy_spec();
    assert_eq!(spec.density, Density::Hd);
    assert_eq!(spec.container, Container::ExtendedAdf);
    assert_eq!(spec.filesystem, Some(FileSystem::FFS));
    assert_eq!(spec.label, "Scratch");
    assert!(spec.bootable);

    let path = scratch("floppy.adf");
    crate::diskimage::create_floppy(&path, &spec).expect("floppy written");
    let adf = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        &adf[0..8],
        b"UAE-1ADF",
        "the extended container was asked for"
    );
    // An extended image is not a plain sector run, so the volume is
    // checked through a plain one written from the same settings.
    let mut plain = spec.clone();
    plain.container = Container::Adf;
    let path = scratch("floppy2.adf");
    crate::diskimage::create_floppy(&path, &plain).expect("floppy written");
    let adf = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(adf.len(), 1_802_240, "HD density");
    assert_eq!(&adf[0..4], b"DOS\x01", "FFS");
    assert!(adf[12..24].iter().any(|&b| b != 0), "boot code was written");
    let root = 1760u64;
    assert_eq!(adf[root as usize * 512 + (128 - 20) * 4], 7, "name length");
    assert_eq!(
        &adf[root as usize * 512 + (128 - 20) * 4 + 1..][..7],
        b"Scratch"
    );

    // --- the hard disk page ---
    let mut state = LauncherState::new(MachineSetup::default());
    state.tab = LauncherTab::CreateHard;
    typed(&mut state, F::NewHardSize, "1234");
    assert_eq!(state.workshop.size, 1234, "the typed size took");
    state.workshop.flip_size_unit(); // MB -> GB
    state.workshop.flip_size_unit(); // and back, so the run stays quick
    typed(&mut state, F::NewHardSize, "48");
    // OFS with international case folding: DOS2.
    state.workshop_set_fs_family(F::NewHardFs, FsFamily::Ofs);
    state.workshop_set_fs_variant(F::NewHardFs, crate::diskimage::Variant::Intl);
    typed(&mut state, F::NewHardDevice, "WORK");
    typed(&mut state, F::NewHardLabel, "Stuff");
    typed(&mut state, F::NewHardBootPri, "-9");
    state.workshop_toggle_flip(F::NewHardReadOnly);

    // Hand-set geometry, reached the way the page reaches it.
    state.workshop.geometry_from_size();
    state.workshop.geometry_custom = true;
    state.tab = LauncherTab::CreateGeometry;
    typed(&mut state, F::NewGeomCylinders, "300");
    typed(&mut state, F::NewGeomSurfaces, "4");
    typed(&mut state, F::NewGeomSectors, "63");
    typed(&mut state, F::NewGeomReserved, "6");

    let spec = state.workshop.hard_spec();
    assert_eq!(spec.partitioning, Partitioning::Rdb);
    assert_eq!(
        spec.filesystem,
        Some(crate::diskimage::FileSystem {
            ffs: false,
            variant: crate::diskimage::Variant::Intl,
        })
    );
    assert_eq!(spec.device, "WORK");
    assert_eq!(spec.label, "Stuff");
    assert_eq!(spec.boot_pri, -9);
    assert_eq!(spec.reserved, 6);
    assert!(spec.read_only);
    assert_eq!(
        spec.geometry,
        Some(crate::diskimage::Geometry {
            cylinders: 300,
            surfaces: 4,
            sectors: 63,
        })
    );

    let path = scratch("hard.hdf");
    let made = crate::diskimage::create_hard(&path, &spec).expect("hard disk written");
    let hdf = std::fs::read(&path).unwrap();
    // Written read-only, so it has to be made writable again to go.
    let perms = std::fs::metadata(&path).unwrap().permissions();
    assert!(perms.readonly(), "the file was marked read only");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(not(unix))]
    {
        let mut perms = perms;
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    let _ = std::fs::remove_file(&path);

    // The geometry decides the size, not the Size box, once it is set
    // by hand: 300 x 4 x 63 blocks of 512.
    assert_eq!(made.bytes, 300 * 4 * 63 * 512);
    assert_eq!(hdf.len() as u64, made.bytes);

    assert_eq!(&hdf[0..4], b"RDSK");
    assert_eq!(long(&hdf, 0, 16), 300, "cylinders");
    assert_eq!(long(&hdf, 0, 17), 63, "sectors");
    assert_eq!(long(&hdf, 0, 18), 4, "surfaces");

    assert_eq!(&hdf[512..516], b"PART");
    assert_eq!(hdf[512 + 36], 4, "device name length");
    assert_eq!(&hdf[512 + 37..512 + 41], b"WORK");
    assert_eq!(long(&hdf, 1, 5), 1, "bootable");
    assert_eq!(long(&hdf, 1, 38), 6, "de_Reserved");
    assert_eq!(long(&hdf, 1, 47) as i32, -9, "de_BootPri");
    assert_eq!(
        long(&hdf, 1, 48),
        crate::diskimage::FileSystem {
            ffs: false,
            variant: crate::diskimage::Variant::Intl,
        }
        .dos_type()
    );

    // And the volume inside the partition carries the name and tag.
    let cyl_blocks = 4 * 63;
    let first = cyl_blocks;
    let blocks = (300 - 1) * cyl_blocks;
    assert_eq!(&hdf[first as usize * 512..][..4], b"DOS\x02");
    let root = first + blocks / 2;
    assert_eq!(hdf[root as usize * 512 + (128 - 20) * 4], 5);
    assert_eq!(
        &hdf[root as usize * 512 + (128 - 20) * 4 + 1..][..5],
        b"Stuff"
    );

    // --- and with the geometry left on Auto, the Size box is what
    // decides, in whichever unit it is showing.
    let mut state = LauncherState::new(MachineSetup::default());
    state.tab = LauncherTab::CreateHard;
    typed(&mut state, F::NewHardSize, "3");
    state.workshop.flip_size_unit(); // MB -> GB
    assert_eq!(state.workshop.bytes(), 3 * 1024 * 1024 * 1024);
    typed(&mut state, F::NewHardSize, "9");
    state.workshop.flip_size_unit(); // GB -> MB
                                     // No partition table, no filesystem: a brand new mechanism, which
                                     // is also the quickest thing to write.
    while state.workshop.partitioning != Partitioning::None {
        state.workshop_cycle(F::NewHardPartitioning, true);
    }
    state.workshop_set_fs_family(F::NewHardFs, FsFamily::Unformatted);
    state.workshop_toggle_flip(F::NewHardSparse);
    let spec = state.workshop.hard_spec();
    assert_eq!(spec.bytes, 9 * 1024 * 1024);
    assert_eq!(spec.geometry, None, "Auto derives it at write time");
    assert!(!spec.sparse, "the tick was cleared, so it is fully written");

    let path = scratch("plain.hdf");
    let made = crate::diskimage::create_hard(&path, &spec).expect("hard disk written");
    let hdf = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    // Rounded up to the next whole cylinder, never below what was asked.
    assert!(made.bytes >= 9 * 1024 * 1024);
    assert_eq!(hdf.len() as u64, made.bytes);
    assert!(
        hdf.iter().all(|&b| b == 0),
        "an unpartitioned, unformatted drive carries nothing at all"
    );
}

/// Click the DOSType boxes until they describe `want`, from whatever
/// they were describing. A directory scheme turns the other away while
/// it is held, so getting from one to the other means clearing first --
/// which is what the page makes you do too.
fn set_dostype(state: &mut LauncherState, field: LauncherField, want: crate::diskimage::Variant) {
    use crate::diskimage::Variant as V;
    for clear in [V::DirCache, V::LongName, V::Intl] {
        if state.workshop_fs_variant_set(field, clear)
            && state.workshop_fs_variant_enabled(field, clear)
        {
            state.workshop_set_fs_variant(field, clear);
        }
    }
    for set in [V::Intl, V::DirCache, V::LongName] {
        let wanted = match set {
            V::Intl => want.is_intl(),
            V::DirCache => want.is_dircache(),
            V::LongName => want.is_longname(),
            V::Plain => false,
        };
        if wanted && !state.workshop_fs_variant_set(field, set) {
            state.workshop_set_fs_variant(field, set);
        }
    }
    if let Some(fs) = state.workshop_fs_of(field) {
        assert_eq!(fs.variant, want, "clicked to the wrong DOS type");
    }
}

/// The filesystem picker is two rows of ticks: the family, then the
/// variants AmigaDOS's own filesystem carries. Between them they have to
/// reach every DOS tag, and never make one that does not exist.
#[test]
fn the_filesystem_ticks_reach_every_dos_tag() {
    use crate::diskimage::Variant;
    let mut state = LauncherState::new(MachineSetup::default());

    // Every tag DOS0..DOS7, from a family tick plus a variant tick.
    let mut seen = std::collections::BTreeSet::new();
    for family in [FsFamily::Ofs, FsFamily::Ffs] {
        for variant in [
            Variant::Plain,
            Variant::Intl,
            Variant::DirCache,
            Variant::LongName,
        ] {
            state.workshop_set_fs_family(F::NewHardFs, family);
            set_dostype(&mut state, F::NewHardFs, variant);
            let fs = state.workshop.hard_fs.expect("a family was chosen");
            assert_eq!(FsFamily::of(Some(fs)), family);
            assert_eq!(fs.variant, variant);
            seen.insert(fs.dos_type());
        }
    }
    assert_eq!(seen.len(), 8, "the two rows reach all eight tags");
    assert_eq!(*seen.first().unwrap(), 0x444F5300);
    assert_eq!(*seen.last().unwrap(), 0x444F5307);

    // Ticking the box that is already set clears it back to plain.
    set_dostype(&mut state, F::NewHardFs, Variant::Plain);
    state.workshop_set_fs_family(F::NewHardFs, FsFamily::Ffs);
    state.workshop_set_fs_variant(F::NewHardFs, Variant::Intl);
    assert!(state.workshop_fs_variant_set(F::NewHardFs, Variant::Intl));
    state.workshop_set_fs_variant(F::NewHardFs, Variant::Intl);
    assert!(!state.workshop_fs_variant_set(F::NewHardFs, Variant::Intl));

    // Moving between OFS and FFS keeps the variant: it is one bit of the
    // tag, and dropping the other two with it would surprise.
    set_dostype(&mut state, F::NewHardFs, Variant::DirCache);
    state.workshop_set_fs_family(F::NewHardFs, FsFamily::Ofs);
    assert!(state.workshop_fs_variant_set(F::NewHardFs, Variant::DirCache));
    assert_eq!(state.workshop.hard_fs.unwrap().dos_type(), 0x444F5304);

    // Unformatted has no DOS type, so the whole identifiers row goes
    // -- its label with it -- and none of the boxes is lit.
    state.workshop_set_fs_family(F::NewHardFs, FsFamily::Unformatted);
    assert_eq!(state.workshop.hard_fs, None);
    assert!(!FsFamily::of(None).has_identifiers());
    assert!(!state.workshop_applies(F::NewHardFsVariant));
    assert!(state.workshop_applies(F::NewHardFs), "the family row stays");
    for variant in [Variant::Intl, Variant::DirCache, Variant::LongName] {
        assert!(!state.workshop_fs_variant_set(F::NewHardFs, variant));
        // ...and clicking one while unformatted does nothing at all.
        state.workshop_set_fs_variant(F::NewHardFs, variant);
        assert_eq!(state.workshop.hard_fs, None);
    }

    // The two pages keep their own choice.
    state.workshop_set_fs_family(F::NewFloppyFs, FsFamily::Ofs);
    state.workshop_set_fs_family(F::NewHardFs, FsFamily::Ffs);
    assert!(state.workshop_fs_family_set(F::NewFloppyFs, FsFamily::Ofs));
    assert!(state.workshop_fs_family_set(F::NewHardFs, FsFamily::Ffs));
    assert!(state.workshop_fs_family_set(F::NewHardFsVariant, FsFamily::Ffs));
}

/// The DOSType boxes are a picture of the tag, so what they show and
/// what they accept both come from the tag rather than from a table
/// written out beside them.
#[test]
fn the_dostype_boxes_agree_with_the_tag_they_describe() {
    use crate::diskimage::Variant as V;
    const BOXES: [V; 3] = [V::Intl, V::DirCache, V::LongName];
    let mut state = LauncherState::new(MachineSetup::default());
    state.workshop_set_fs_family(F::NewHardFs, FsFamily::Ffs);

    // Whatever tag is held, each box shows exactly what that tag says
    // about itself, and offers a click only where another tag is
    // reachable by changing that one box.
    for held in V::ALL {
        state.workshop.hard_fs = Some(crate::diskimage::FileSystem {
            ffs: true,
            variant: held,
        });
        for boxed in BOXES {
            let shown = state.workshop_fs_variant_set(F::NewHardFs, boxed);
            let says = match boxed {
                V::Intl => held.is_intl(),
                V::DirCache => held.is_dircache(),
                V::LongName => held.is_longname(),
                V::Plain => unreachable!("not a box"),
            };
            assert_eq!(shown, says, "{held:?}: the {boxed:?} box");

            // Clicking is offered only when it lands somewhere: a
            // directory scheme carries international with it, so that
            // box cannot be cleared while one is chosen, and the two
            // schemes are one field so neither can join the other.
            let offered = state.workshop_fs_variant_enabled(F::NewHardFs, boxed);
            let reachable = match boxed {
                V::Intl => !held.is_dircache() && !held.is_longname(),
                V::DirCache => !held.is_longname(),
                V::LongName => !held.is_dircache(),
                V::Plain => unreachable!("not a box"),
            };
            assert_eq!(offered, reachable, "{held:?}: the {boxed:?} box");

            // A click the page will not offer changes nothing.
            if !offered {
                state.workshop_set_fs_variant(F::NewHardFs, boxed);
                assert_eq!(state.workshop.hard_fs.unwrap().variant, held);
            }
        }
    }

    // Every tag is reachable by clicking, and no click ever lands on a
    // combination that is not one: three boxes have eight states, the
    // field has four, and the four are the ones the page can produce.
    let mut reached = std::collections::HashSet::new();
    for first in BOXES {
        for second in BOXES {
            for third in BOXES {
                state.workshop.hard_fs = Some(crate::diskimage::FileSystem {
                    ffs: true,
                    variant: V::Plain,
                });
                for click in [first, second, third] {
                    state.workshop_set_fs_variant(F::NewHardFs, click);
                }
                let held = state.workshop.hard_fs.unwrap().variant;
                // Never both schemes at once, however the clicks fell.
                assert!(!(held.is_dircache() && held.is_longname()));
                reached.insert(held);
            }
        }
    }
    assert_eq!(
        reached,
        V::ALL.into_iter().collect::<std::collections::HashSet<_>>(),
        "three boxes reach all four tags and nothing else"
    );
}

/// The drive identity: what it says by default, what typing into it
/// does, and that a field never spills into the one beside it.
#[test]
fn the_drive_identity_names_itself_until_it_is_told_otherwise() {
    let mut state = LauncherState::new(MachineSetup::default());
    state.tab = LauncherTab::CreateGeometry;

    // Untouched, the drive names itself from its size -- and follows
    // the Size box rather than going stale behind it.
    assert_eq!(state.workshop.identity().vendor, "Amiga");
    assert_eq!(state.workshop.identity().product, "64MB HDF");
    state.workshop.size = 2;
    state.workshop.size_unit = SizeUnit::Gb;
    assert_eq!(state.workshop.identity().product, "2GB HDF");

    // The revision is Copperline's own version, cut to the four bytes
    // the field holds.
    let revision = state.workshop.identity().revision;
    assert!(revision.len() <= crate::harddrive::RDB_IDENTITY_WIDTHS[2]);
    assert!(env!("CARGO_PKG_VERSION").starts_with(&revision));

    // Typing into one field leaves the others deriving, so a Drive
    // typed now does not freeze the Type at today's size.
    state.begin_edit_new_image(F::NewGeomVendor);
    while !state.edit_buffer().is_empty() {
        state.edit_backspace();
    }
    for c in "A600 HD".chars() {
        state.edit_push(c);
    }
    state.edit_commit();
    assert_eq!(state.editing(), None);
    assert_eq!(state.workshop.identity().vendor, "A600 HD");
    assert_eq!(state.workshop.identity().product, "2GB HDF");
    // ...and the Type keeps following the size afterwards, which is the
    // whole reason each field is remembered separately.
    state.workshop.size = 512;
    state.workshop.size_unit = SizeUnit::Mb;
    assert_eq!(state.workshop.identity().vendor, "A600 HD");
    assert_eq!(state.workshop.identity().product, "512MB HDF");

    // Each box stops at the width its RDB field has: a longer string
    // would spill into the next field rather than simply being long.
    for (field, width) in [
        (F::NewGeomVendor, crate::harddrive::RDB_IDENTITY_WIDTHS[0]),
        (F::NewGeomProduct, crate::harddrive::RDB_IDENTITY_WIDTHS[1]),
        (F::NewGeomRevision, crate::harddrive::RDB_IDENTITY_WIDTHS[2]),
    ] {
        state.begin_edit_new_image(field);
        while !state.edit_buffer().is_empty() {
            state.edit_backspace();
        }
        for c in "0123456789ABCDEFGHIJ".chars() {
            state.edit_push(c);
        }
        assert_eq!(state.edit_buffer().chars().count(), width, "{field:?}");
        // And nothing a tool could not print back gets in at all.
        for c in ['\n', '\t', 'é'] {
            let before = state.edit_buffer().to_string();
            state.edit_push(c);
            assert_eq!(state.edit_buffer(), before, "{field:?} took {c:?}");
        }
        state.edit_commit();
    }

    // Auto puts the whole page back to naming itself.
    state.workshop.geometry_from_size();
    assert_eq!(state.workshop.vendor, None);
    assert_eq!(state.workshop.product, None);
    assert_eq!(state.workshop.revision, None);
    assert_eq!(state.workshop.identity().vendor, "Amiga");
    assert_eq!(state.workshop.identity().product, "512MB HDF");
}

#[test]
fn sub_pages_of_hdd_cd() {
    // The sub-pages (CD included) are not top-level strip tabs.
    for t in [
        LauncherTab::Cd,
        LauncherTab::HostFs,
        LauncherTab::HostDisk,
        LauncherTab::BootPriority,
        LauncherTab::Lide,
        LauncherTab::Copperhf,
    ] {
        assert!(!TABS.contains(&t));
        // Each keeps the Storage strip tab highlighted and returns to it.
        assert_eq!(t.strip_tab(), LauncherTab::Storage);
        assert_eq!(t.parent_tab(), Some(LauncherTab::Storage));
    }
    // A top-level tab has no parent.
    assert_eq!(LauncherTab::Storage.parent_tab(), None);

    // The Storage nav lists its sub-pages in order (drawn as a fixed top
    // nav row, not a settings row). Create Image lands on the floppy
    // page, which is the workshop's default half.
    let storage_nav: Vec<_> = LauncherTab::Storage
        .nav_options()
        .iter()
        .map(|&(_, t)| t)
        .collect();
    assert_eq!(
        storage_nav,
        [
            LauncherTab::Cd,
            LauncherTab::HostFs,
            LauncherTab::HostDisk,
            LauncherTab::Lide,
            LauncherTab::Copperhf,
            LauncherTab::BootPriority,
            LauncherTab::CreateFloppy,
        ]
    );

    // The two workshop pages are siblings of each other and children of
    // Storage: their nav row carries a Back button *and* the pair, so a
    // page says both where it came from and which of the two it is.
    for tab in [LauncherTab::CreateFloppy, LauncherTab::CreateHard] {
        assert_eq!(tab.parent_tab(), Some(LauncherTab::Storage));
        assert_eq!(tab.strip_tab(), LauncherTab::Storage);
        assert!(!TABS.contains(&tab), "a workshop page is not a strip tab");
        let nav: Vec<_> = tab.nav_options().iter().map(|&(_, t)| t).collect();
        assert_eq!(
            nav,
            [LauncherTab::CreateFloppy, LauncherTab::CreateHard],
            "{} does not offer both halves",
            tab.label()
        );
    }

    // The Storage tab is just the storage rows (IDE/SCSI options).
    let storage = rows(
        LauncherTab::Storage,
        ParallelDevice::None,
        SerialMode::default(),
        false,
        false,
    );
    assert_eq!(
        storage.first().map(|r| r.field),
        Some(LauncherField::IdeMaster)
    );

    // Each sub-page carries its own rows (its Back button is a fixed top nav
    // element, not a settings row).
    for (tab, marker) in [
        (LauncherTab::HostFs, LauncherField::Filesys0Dir),
        (LauncherTab::Cd, LauncherField::CdImage),
        (LauncherTab::BootPriority, LauncherField::IdeMasterBoot),
        (LauncherTab::Lide, LauncherField::LideBoard),
    ] {
        let page = rows(
            tab,
            ParallelDevice::None,
            SerialMode::default(),
            false,
            false,
        );
        assert!(page.iter().any(|r| r.field == marker));
    }
}

#[test]
fn av_emu_categories() {
    use LauncherField as F;
    // Only "A/V & Emu" (the Audio default) is a strip tab; Video, Display,
    // Emulation and Paths are its categories.
    assert!(TABS.contains(&LauncherTab::AvAudio));
    assert!(!TABS.contains(&LauncherTab::AvVideo));
    assert!(!TABS.contains(&LauncherTab::AvDisplay));
    assert!(!TABS.contains(&LauncherTab::AvEmulation));
    assert!(!TABS.contains(&LauncherTab::AvPaths));
    for t in [
        LauncherTab::AvVideo,
        LauncherTab::AvDisplay,
        LauncherTab::AvEmulation,
        LauncherTab::AvPaths,
    ] {
        // They keep the A/V strip entry lit and have no Back button --
        // categories switch between each other via the top nav row.
        assert_eq!(t.strip_tab(), LauncherTab::AvAudio);
        assert_eq!(t.parent_tab(), None);
    }
    // Every A/V page offers the same nav buttons.
    let nav = LauncherTab::AvAudio.nav_options();
    assert_eq!(
        nav,
        [
            ("Audio", LauncherTab::AvAudio),
            ("Video", LauncherTab::AvVideo),
            ("Display", LauncherTab::AvDisplay),
            ("Emulation", LauncherTab::AvEmulation),
            ("Paths", LauncherTab::AvPaths),
        ]
    );
    assert_eq!(LauncherTab::AvVideo.nav_options(), nav);
    assert_eq!(LauncherTab::AvDisplay.nav_options(), nav);
    assert!(LauncherTab::AvAudio.has_top_nav());
    assert!(LauncherTab::Storage.has_top_nav());
    assert!(!LauncherTab::System.has_top_nav());

    // Each category shows only its own settings; the default is Audio.
    let page = |t| rows(t, ParallelDevice::None, SerialMode::default(), false, false);
    let audio = page(LauncherTab::AvAudio);
    assert!(audio.iter().any(|r| r.field == F::AudioDevice));
    assert!(audio.iter().all(|r| r.field != F::StartFullscreen));
    // Video is the emulated picture; Display is the host window. The
    // fullscreen switch is the window's, the shader the picture's.
    assert!(page(LauncherTab::AvVideo)
        .iter()
        .any(|r| r.field == F::Shader));
    assert!(page(LauncherTab::AvVideo)
        .iter()
        .all(|r| r.field != F::StartFullscreen));
    assert!(page(LauncherTab::AvDisplay)
        .iter()
        .any(|r| r.field == F::StartFullscreen));
    assert!(page(LauncherTab::AvDisplay)
        .iter()
        .all(|r| r.field != F::Bezel));
    assert!(page(LauncherTab::AvEmulation)
        .iter()
        .any(|r| r.field == F::PowerOn));
}

#[test]
fn scsi_rows_hidden_until_a_controller_is_chosen() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    let scsi_rows = [
        F::ScsiRom,
        F::ScsiRomOdd,
        F::ScsiUnit0,
        F::ScsiUnit1,
        F::ScsiUnit2,
        F::ScsiUnit3,
        F::ScsiUnit4,
        F::ScsiUnit5,
        F::ScsiUnit6,
    ];
    // No controller: the ROM and unit rows go rather than stand greyed.
    assert_eq!(s.scsi_controller, None);
    for f in scsi_rows {
        assert!(s.row_hidden(f), "{f:?} shown with no controller");
    }
    // Choosing one brings every row back...
    s.cycle(F::ScsiController, true);
    assert!(s.scsi_controller.is_some());
    for f in scsi_rows {
        assert!(!s.row_hidden(f), "{f:?} hidden with a controller fitted");
    }
    // ...and cycling back to None takes them away again.
    s.cycle(F::ScsiController, false);
    assert_eq!(s.scsi_controller, None);
    for f in scsi_rows {
        assert!(s.row_hidden(f), "{f:?} shown after the controller left");
    }
}

#[test]
fn auto_launch_round_trips_and_defaults_off() {
    use LauncherField as F;
    // Off by default, and an untouched setup writes no key.
    let s = MachineSetup::default();
    assert!(!s.auto_launch());
    assert!(s.to_raw().emulation.auto_launch.is_none());

    // Toggled on, it survives the round trip.
    let mut s = s;
    s.cycle(F::AutoLaunch, true);
    assert!(s.auto_launch());
    let raw = s.to_raw();
    assert_eq!(raw.emulation.auto_launch, Some(true));
    assert!(MachineSetup::from_raw(&raw).unwrap().auto_launch());

    // And the row sits directly below Power on startup on the Emulation
    // page, as its sibling.
    let page = rows(
        LauncherTab::AvEmulation,
        ParallelDevice::None,
        SerialMode::default(),
        false,
        false,
    );
    let power = page.iter().position(|r| r.field == F::PowerOn).unwrap();
    assert_eq!(page[power + 1].field, F::AutoLaunch);
}

#[test]
fn floppy_rows_hidden_until_wired() {
    use LauncherField as F;
    let with_drives = |n: u8| {
        MachineSetup::from_raw(&toml::from_str(&format!("[floppy]\ndrives = {n}")).unwrap())
            .unwrap()
    };
    let zero = with_drives(0);
    assert!(zero.row_hidden(F::Df0Image));
    assert!(zero.disabled_reason(F::FloppySpeed).is_some());
    let one = with_drives(1);
    assert!(!one.row_hidden(F::Df0Image));
    assert!(one.row_hidden(F::Df1Image));
    assert!(one.row_hidden(F::Df3WriteProtect));
    let three = with_drives(3);
    assert!(!three.row_hidden(F::Df1Image));
    assert!(!three.row_hidden(F::Df2WriteProtect));
    assert!(three.row_hidden(F::Df3Image));
}

#[test]
fn shader_strength_greys_when_shader_off() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    // The shader is off by default, so its strength does nothing and greys.
    assert_eq!(s.value_label(F::Shader), "Disabled");
    assert_eq!(s.disabled_reason(F::ShaderStrength), Some("Disabled"));
    assert!(!s.applies(F::ShaderStrength));
    // Turning a shader on makes the strength editable again.
    s.cycle(F::Shader, true); // Off -> the first real shader
    assert_ne!(s.value_label(F::Shader), "Disabled");
    assert_eq!(s.disabled_reason(F::ShaderStrength), None);
}

#[test]
fn boot_priority_round_trips_and_greys_empty_slots() {
    use LauncherField as F;
    // A drive carrying a bootpri loads it; an empty slot is greyed.
    let raw: RawConfig = toml::from_str(
        r#"
            [machine]
            model = "A1200"
            [ide]
            master = { path = "wb.hdf", bootpri = 5 }
        "#,
    )
    .unwrap();
    let mut setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.value_label(F::IdeMasterBoot), "5");
    assert!(!setup.drive_boot_off(F::IdeMasterBoot));
    assert_eq!(setup.disabled_reason(F::IdeMasterBoot), None);
    // With a drive present the page has editable rows (its info text shows);
    // a machine with no hard-disk drives has none.
    assert!(setup.has_boot_priority_rows());
    assert!(!MachineSetup::default().has_boot_priority_rows());
    // No slave image, so its boot row is greyed and inert.
    assert_eq!(setup.disabled_reason(F::IdeSlaveBoot), Some("No drive"));

    // The arrows step the live priority and re-emit it.
    setup.cycle(F::IdeMasterBoot, true); // 5 -> 6
    assert_eq!(setup.value_label(F::IdeMasterBoot), "6");
    assert_eq!(setup.to_raw().ide.master.as_ref().unwrap().bootpri, Some(6));

    // Unset (None) shows as 0 and stays keyless on save.
    setup.set_drive_bootpri(F::IdeMasterBoot, None);
    assert_eq!(setup.value_label(F::IdeMasterBoot), "0");
    assert_eq!(setup.to_raw().ide.master.as_ref().unwrap().bootpri, None);

    // Clearing the Bootable box writes the -128 sentinel and shows it, so
    // the greyed row says what the config will hold; the priority is kept
    // underneath, and re-ticking restores it.
    setup.set_drive_bootpri(F::IdeMasterBoot, Some(6));
    setup.toggle_drive_boot(F::IdeMasterBoot);
    assert!(setup.drive_boot_off(F::IdeMasterBoot));
    assert_eq!(setup.value_label(F::IdeMasterBoot), "-128");
    assert_eq!(
        setup.to_raw().ide.master.as_ref().unwrap().bootpri,
        Some(-128)
    );
    setup.toggle_drive_boot(F::IdeMasterBoot);
    assert_eq!(setup.value_label(F::IdeMasterBoot), "6");
    assert_eq!(setup.to_raw().ide.master.as_ref().unwrap().bootpri, Some(6));
}

#[test]
fn boot_priority_arrows_update_every_listed_drive_and_survive_config_save() {
    for row in BOOTPRI_ROWS {
        let field = row.field;
        let mut setup = MachineSetup::default();
        setup.select_model(Some(MachineModel::A1200));
        setup.scsi_controller = Some(ScsiController::A2091);
        setup.lide_board = Some(LidePersonality::Ripple);
        let drive = MachineSetup::boot_field_drive(field).expect("boot row has a drive");
        setup.set_path(drive, PathBuf::from("disk.hdf"));
        setup.set_drive_bootpri(field, Some(5));

        setup.cycle(field, true);
        assert_eq!(setup.value_label(field), "6", "{field:?}");
        let saved = MachineSetup::from_raw(&setup.to_raw()).unwrap();
        assert_eq!(saved.value_label(field), "6", "{field:?}");
        setup.cycle(field, false);
        assert_eq!(setup.value_label(field), "5", "{field:?}");

        setup.toggle_drive_boot(field);
        setup.cycle(field, true);
        assert_eq!(setup.value_label(field), "-128", "{field:?}");
        setup.toggle_drive_boot(field);
        assert_eq!(setup.value_label(field), "5", "{field:?}");
    }
}

#[test]
fn boot_priority_loads_the_never_sentinel_as_a_cleared_box() {
    use LauncherField as F;
    let raw: RawConfig = toml::from_str(
        r#"
            [machine]
            model = "A1200"
            [ide]
            master = { path = "wb.hdf", bootpri = -128 }
        "#,
    )
    .unwrap();
    let setup = MachineSetup::from_raw(&raw).unwrap();
    assert!(setup.drive_boot_off(F::IdeMasterBoot));
    assert_eq!(
        setup.to_raw().ide.master.as_ref().unwrap().bootpri,
        Some(-128)
    );
}

#[test]
fn boot_priority_cascade_seeds_added_drives() {
    use LauncherField as F;
    let mut setup = MachineSetup::default();
    setup.select_model(Some(MachineModel::A1200));
    // First drive added takes 0 (keyless); the second drops below the
    // floppies to -35, then -40, each written explicitly.
    setup.set_path(F::IdeMaster, PathBuf::from("a.hdf"));
    setup.set_path(F::IdeSlave, PathBuf::from("b.hdf"));
    assert_eq!(setup.value_label(F::IdeMasterBoot), "0");
    assert_eq!(setup.value_label(F::IdeSlaveBoot), "-35");
    assert_eq!(setup.to_raw().ide.master.as_ref().unwrap().bootpri, None);
    assert_eq!(
        setup.to_raw().ide.slave.as_ref().unwrap().bootpri,
        Some(-35)
    );
}

#[test]
fn boot_priority_steps_clamp_and_parse() {
    // The arrows stay within -127..=127 (the -128 sentinel is the box).
    assert_eq!(step_drive_bootpri(Some(127), true), Some(127));
    assert_eq!(step_drive_bootpri(Some(-127), false), Some(-127));
    // Unset steps off from 0.
    assert_eq!(step_drive_bootpri(None, true), Some(1));
    assert_eq!(step_drive_bootpri(None, false), Some(-1));
    // The Priority column shows the number, 0 when unset.
    assert_eq!(drive_bootpri_label(None), "0");
    assert_eq!(drive_bootpri_label(Some(7)), "7");
    // Cascade: rank 0 keyless, then -35, -40, -45.
    assert_eq!(hdd_boot_cascade(0), None);
    assert_eq!(hdd_boot_cascade(1), Some(-35));
    assert_eq!(hdd_boot_cascade(3), Some(-45));
    // Typed entry: blank clears to unset, range enforced, -128 accepted
    // (the commit turns it into a cleared box).
    assert_eq!(parse_drive_bootpri("  "), Ok(None));
    assert_eq!(parse_drive_bootpri("-128"), Ok(Some(-128)));
    assert!(parse_drive_bootpri("200").is_err());
}

#[test]
fn lide_board_cycles_and_round_trips() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    assert_eq!(s.value_label(F::LideBoard), "None");
    assert!(s.to_raw().lide.board.is_none());

    s.cycle(F::LideBoard, true);
    assert_eq!(s.value_label(F::LideBoard), "RIPPLE");
    s.cycle(F::LideBoard, true);
    assert_eq!(s.value_label(F::LideBoard), "RIDE");
    s.cycle(F::LideBoard, true);
    assert_eq!(s.value_label(F::LideBoard), "AT-Bus 2008");

    // A bare board with nothing attached is indistinguishable from no
    // board at all (`LideConfig::enabled`, matching `[scsi]`), so give it
    // a ROM before checking the round trip.
    s.set_path(F::LideRom, PathBuf::from("lide-atbus.rom"));
    let raw = s.to_raw();
    assert_eq!(raw.lide.board.as_deref(), Some("atbus2008"));
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.value_label(F::LideBoard), "AT-Bus 2008");

    s.cycle(F::LideBoard, true);
    assert_eq!(s.value_label(F::LideBoard), "None");
    assert!(s.to_raw().lide.board.is_none());
}

#[test]
fn lide_rows_are_hidden_without_a_board_and_rom_bank2_hidden_on_atbus2008() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    // Nothing to configure without a board fitted.
    assert!(s.row_hidden(F::LideRom));
    assert!(s.row_hidden(F::LideRomBank2));
    assert!(s.row_hidden(F::LideDrive0));
    assert!(s.row_hidden(F::LideDrive1));

    s.cycle(F::LideBoard, true); // RIPPLE: two channels, four drives
    assert!(!s.row_hidden(F::LideRom));
    assert!(!s.row_hidden(F::LideRomBank2));
    // Every slot the fitted board has is configurable straight away, the
    // same as `[ide]`'s master/slave and `[scsi]`'s units: each is its own
    // `driveN` key, so a hole is representable and nothing is gated on the
    // slot before it holding an image.
    assert!(!s.row_hidden(F::LideDrive0));
    assert!(!s.row_hidden(F::LideDrive1));
    assert!(!s.row_hidden(F::LideDrive2));
    assert!(!s.row_hidden(F::LideDrive3));

    s.cycle(F::LideBoard, true); // RIDE: one channel, two drives, four banks
    assert!(!s.row_hidden(F::LideRomBank2), "RIDE has flash banking too");
    assert!(s.row_hidden(F::LideDrive2), "RIDE has only one channel");
    assert!(s.row_hidden(F::LideDrive3));

    s.cycle(F::LideBoard, true); // AT-Bus 2008: one channel, no banking
    assert!(s.row_hidden(F::LideRomBank2));
    assert!(s.row_hidden(F::LideDrive2));
}

#[test]
fn lide_rom_defaults_without_leaking_the_bundled_sentinel() {
    use LauncherField as F;
    // A fitted board with no rom named: `from_raw` runs the same
    // `Config::try_from` that fills the bundled-rom sentinel in, which
    // must never reach the launcher's own path field -- it would both
    // display as literal text and, on save, get pinned into the file.
    let raw = RawConfig::parse(
        r#"
        [lide]
        board = "ripple"
        drives = ["dh0.hdf"]
        "#,
    )
    .unwrap();
    let s = MachineSetup::from_raw(&raw).unwrap();
    assert!(s.path(F::LideRom).is_none());
    assert!(s.path(F::LideRomBank2).is_none());
    assert_eq!(s.value_label(F::LideRom), "(bundled lide.rom)");
    assert_eq!(s.value_label(F::LideRomBank2), "(bundled cdfs.rom)");

    // Saving without touching the field leaves rom unset -- the bundled
    // default keeps applying, rather than pinning the sentinel string.
    let saved = s.to_raw();
    assert!(saved.lide.rom.is_none());
    assert!(saved.lide.rom_bank2.is_none());
}

#[test]
fn lide_rom_explicit_opt_out_survives_a_launcher_round_trip() {
    use LauncherField as F;
    let raw = RawConfig::parse(
        r#"
        [lide]
        board = "ripple"
        rom = ""
        drives = ["dh0.hdf"]
        "#,
    )
    .unwrap();
    let s = MachineSetup::from_raw(&raw).unwrap();
    assert!(s.path(F::LideRom).is_none());
    assert_eq!(s.value_label(F::LideRom), "(hardware-only, no ROM)");

    // Untouched, the explicit opt-out is preserved rather than silently
    // dropped back to the implicit bundled default.
    let saved = s.to_raw();
    assert_eq!(saved.lide.rom.as_deref(), Some(""));

    // Typing a path un-disables it, same as any other field.
    let mut s = s;
    s.set_path(F::LideRom, PathBuf::from("custom-lide.rom"));
    assert_eq!(s.to_raw().lide.rom.as_deref(), Some("custom-lide.rom"));
}

#[test]
fn lide_drives_round_trip_in_channel_order_with_boot_priority() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    s.cycle(F::LideBoard, true); // RIPPLE
    s.set_path(F::LideDrive0, PathBuf::from("ch0-master.hdf"));
    s.set_path(F::LideDrive1, PathBuf::from("ch0-slave.hdf"));
    s.set_drive_bootpri(F::LideDrive0Boot, Some(5));

    // Boot priority lives on the shared Boot Priority page with every other
    // drive's, so a filled Lide slot puts a row there.
    assert!(s.has_boot_priority_rows());
    assert_eq!(s.value_label(F::LideDrive0Boot), "5");
    assert_eq!(s.disabled_reason(F::LideDrive1Boot), None);
    // Two IDE bays, which stand their ground empty, and the two Lide slots
    // now carrying a disk. No SCSI controller is fitted here.
    assert_eq!(s.boot_priority_row_count(), 4);
    // An empty slot has nothing to order, so it takes no place in the list
    // -- the same terms a SCSI unit is listed on.
    assert!(s.row_hidden(F::LideDrive2Boot));
    // A slot the fitted board does not have goes entirely, drive row and
    // boot row together.
    s.cycle(F::LideBoard, true); // RIDE: one channel
    assert!(s.row_hidden(F::LideDrive2));
    assert!(s.row_hidden(F::LideDrive2Boot));
    // The Lide page itself is drives and ROM only now.
    assert!(!rows(
        LauncherTab::Lide,
        Default::default(),
        Default::default(),
        false,
        false
    )
    .iter()
    .any(|r| r.kind == RowKind::Bootpri));
    s.cycle(F::LideBoard, true); // AT-Bus 2008
    s.cycle(F::LideBoard, true); // None
    s.cycle(F::LideBoard, true); // back to RIPPLE for the round trip below

    let raw = s.to_raw();
    assert_eq!(raw.lide.board.as_deref(), Some("ripple"));
    // Named per-slot keys, never the deprecated positional array.
    assert!(raw.lide.drives.is_empty());
    assert_eq!(raw.lide.drive0.as_ref().unwrap().path, "ch0-master.hdf");
    assert_eq!(raw.lide.drive0.as_ref().unwrap().bootpri, Some(5));
    assert_eq!(raw.lide.drive1.as_ref().unwrap().path, "ch0-slave.hdf");
    assert!(raw.lide.drive2.is_none());

    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.path(F::LideDrive0), Some(Path::new("ch0-master.hdf")));
    assert_eq!(back.path(F::LideDrive1), Some(Path::new("ch0-slave.hdf")));
    assert_eq!(back.value_label(F::LideDrive0Boot), "5");
}

/// Unlike SCSI (gated behind a chosen controller) and Lide (gated behind a
/// chosen board), `[copperhf]` has no board/personality to pick -- the
/// board is always there -- so its seven unit rows are always shown and
/// never greyed for that reason, on a fresh `MachineSetup` with no model
/// selected at all.
#[test]
fn copperhf_units_are_always_visible_and_never_greyed() {
    use LauncherField as F;
    let s = MachineSetup::default();
    let unit_rows = [
        F::CopperhfUnit0,
        F::CopperhfUnit1,
        F::CopperhfUnit2,
        F::CopperhfUnit3,
        F::CopperhfUnit4,
        F::CopperhfUnit5,
        F::CopperhfUnit6,
    ];
    for f in unit_rows {
        assert!(!s.row_hidden(f), "{f:?} hidden with no unit configured");
        assert_eq!(
            s.disabled_reason(f),
            None,
            "{f:?} greyed with no board to lack"
        );
    }
    // The copperhf Storage sub-page itself carries just the seven drive
    // rows -- no board/ROM row above them, unlike Lide's page.
    let page_rows = rows(
        LauncherTab::Copperhf,
        Default::default(),
        Default::default(),
        false,
        false,
    );
    assert_eq!(page_rows.len(), 7);
    assert!(page_rows.iter().all(|r| r.kind == RowKind::Drive));
}

/// `[copperhf]` units round-trip through the config screen like SCSI/Lide
/// units: path, volume name, and boot priority all survive a
/// `to_raw`/`from_raw` cycle, and an empty unit takes no boot-priority row.
#[test]
fn copperhf_units_round_trip_with_boot_priority() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    s.set_path(F::CopperhfUnit0, PathBuf::from("workbench.hdf"));
    s.set_path(F::CopperhfUnit1, PathBuf::from("data.hdf"));
    s.set_drive_bootpri(F::CopperhfUnit0Boot, Some(5));

    assert!(s.has_boot_priority_rows());
    assert_eq!(s.value_label(F::CopperhfUnit0Boot), "5");
    assert_eq!(s.disabled_reason(F::CopperhfUnit1Boot), None);
    // An empty unit has nothing to order, so it takes no place in the list.
    assert!(s.row_hidden(F::CopperhfUnit2Boot));

    let raw = s.to_raw();
    assert_eq!(raw.copperhf.unit0.as_ref().unwrap().path, "workbench.hdf");
    assert_eq!(raw.copperhf.unit0.as_ref().unwrap().bootpri, Some(5));
    assert_eq!(raw.copperhf.unit1.as_ref().unwrap().path, "data.hdf");
    assert!(raw.copperhf.unit2.is_none());

    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(
        back.path(F::CopperhfUnit0),
        Some(Path::new("workbench.hdf"))
    );
    assert_eq!(back.path(F::CopperhfUnit1), Some(Path::new("data.hdf")));
    assert_eq!(back.value_label(F::CopperhfUnit0Boot), "5");
}

/// The Boot Priority page ranks drives with no real-hardware counterpart
/// last: IDE, then SCSI, then Lide, then Copperhf units -- matching
/// `BOOTPRI_ROWS`'s own fixed order.
#[test]
fn copperhf_units_take_their_place_after_lide_in_the_boot_order() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A1200));
    s.set_path(F::IdeMaster, PathBuf::from("ide0.hdf"));
    s.cycle(F::LideBoard, true); // RIPPLE
    s.set_path(F::LideDrive0, PathBuf::from("lide0.hdf"));
    s.set_path(F::CopperhfUnit0, PathBuf::from("copperhf0.hdf"));

    let listed: Vec<_> = rows(
        LauncherTab::BootPriority,
        Default::default(),
        Default::default(),
        false,
        false,
    )
    .iter()
    .filter(|r| s.row_on_page(LauncherTab::BootPriority, r.field))
    .map(|r| r.field)
    .collect();
    assert_eq!(
        listed,
        vec![
            F::SectionHeader,
            F::IdeMasterBoot,
            F::IdeSlaveBoot,
            F::LideDrive0Boot,
            F::CopperhfUnit0Boot,
        ]
    );
    assert_eq!(s.boot_priority_row_count(), 4);
}

#[test]
fn ide_and_lide_cd_images_label_and_grey_boot_priority_like_scsi() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    s.cycle(F::LideBoard, true); // RIPPLE
    s.set_path(F::IdeMaster, PathBuf::from("game.iso"));
    s.set_path(F::LideDrive0, PathBuf::from("game.cue"));

    assert_eq!(s.value_label(F::IdeMaster), "game.iso (CD-ROM)");
    assert_eq!(s.value_label(F::LideDrive0), "game.cue (CD-ROM)");
    assert_eq!(s.disabled_reason(F::IdeMasterBoot), Some("CD-ROM"));
    assert_eq!(s.disabled_reason(F::LideDrive0Boot), Some("CD-ROM"));

    // A hard disk at the same fields gets no such treatment.
    s.set_path(F::IdeSlave, PathBuf::from("work.hdf"));
    assert_eq!(s.value_label(F::IdeSlave), "work.hdf");
    assert_eq!(s.disabled_reason(F::IdeSlaveBoot), None);
}

/// The FFS/OFS toggle applies only to a directory mount on a
/// disk-backed drive field (IDE/SCSI/lide), never to a `Filesys*Dir`
/// row -- a live HOSTFS mount is a directory too, but has no filesystem
/// of its own to choose. `drive_is_directory` is also the cached flag
/// `launcher_drive_fs_applies` (`ui.rs`) reads instead of statting the
/// path on every frame; this exercises both that it is scoped correctly
/// and that it tracks the real path shape as it changes.
#[test]
fn drive_filesystem_toggle_applies_only_to_disk_backed_fields_and_tracks_path_shape() {
    use LauncherField as F;
    let dir = std::env::temp_dir().join(format!(
        "copperline-launcher-isdir-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let mut s = MachineSetup::default();
    // A live HOSTFS mount points at a real directory too, but the
    // toggle must never claim to apply there.
    s.set_path(F::Filesys0Dir, dir.clone());
    assert!(!s.drive_is_directory(F::Filesys0Dir));

    // A disk-backed field pointed at that same directory: applies.
    s.set_path(F::IdeMaster, dir.clone());
    assert!(s.drive_is_directory(F::IdeMaster));

    // Repointed at an ordinary file: does not apply.
    let file = dir.join("plain.hdf");
    std::fs::write(&file, b"").unwrap();
    s.set_path(F::IdeMaster, file);
    assert!(!s.drive_is_directory(F::IdeMaster));

    // Clearing drops the flag along with the path.
    s.set_path(F::IdeMaster, dir.clone());
    assert!(s.drive_is_directory(F::IdeMaster));
    s.clear_path(F::IdeMaster);
    assert!(!s.drive_is_directory(F::IdeMaster));

    std::fs::remove_dir_all(&dir).ok();
}

/// Clearing a slot is just that one slot: with a named `driveN` key each,
/// a gap is representable, so emptying channel 0's master no longer drags
/// everything after it out too.
#[test]
fn lide_clearing_a_drive_leaves_later_slots_alone() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    s.cycle(F::LideBoard, true); // RIPPLE
    s.set_path(F::LideDrive0, PathBuf::from("a.hdf"));
    s.set_path(F::LideDrive1, PathBuf::from("b.hdf"));
    s.set_path(F::LideDrive2, PathBuf::from("c.hdf"));

    s.clear_path(F::LideDrive0);
    assert_eq!(s.path(F::LideDrive0), None);
    assert_eq!(s.path(F::LideDrive1), Some(Path::new("b.hdf")));
    assert_eq!(s.path(F::LideDrive2), Some(Path::new("c.hdf")));

    // ...and the hole survives a round trip through the config.
    let raw = s.to_raw();
    assert!(raw.lide.drive0.is_none(), "the emptied slot stays empty");
    assert_eq!(raw.lide.drive1.as_ref().unwrap().path, "b.hdf");
    assert_eq!(raw.lide.drive2.as_ref().unwrap().path, "c.hdf");
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.path(F::LideDrive0), None);
    assert_eq!(back.path(F::LideDrive1), Some(Path::new("b.hdf")));
    assert_eq!(back.path(F::LideDrive2), Some(Path::new("c.hdf")));
}

/// Lide host-disk attachment points are only fitted for a channel the
/// selected board personality actually has.
#[test]
fn lide_host_disk_attach_points_are_fitted_by_board_channel_count() {
    use crate::config::HostDiskAttach;
    use LauncherField as F;
    let mut s = MachineSetup::default();

    // No board at all: nothing is fitted.
    assert!(!s.attach_is_fitted(HostDiskAttach::LideMaster(0)));

    // RIDE: one channel.
    s.cycle(F::LideBoard, true); // RIPPLE
    s.cycle(F::LideBoard, true); // RIDE
    assert!(s.attach_is_fitted(HostDiskAttach::LideMaster(0)));
    assert!(!s.attach_is_fitted(HostDiskAttach::LideMaster(1)));

    // RIPPLE: two channels.
    s.cycle(F::LideBoard, true); // AT-Bus 2008
    s.cycle(F::LideBoard, true); // None
    s.cycle(F::LideBoard, true); // RIPPLE
    assert!(s.attach_is_fitted(HostDiskAttach::LideMaster(0)));
    assert!(s.attach_is_fitted(HostDiskAttach::LideSlave(1)));

    assert_eq!(
        MachineSetup::host_disk_attach_of(F::LideDrive0),
        Some(HostDiskAttach::LideMaster(0))
    );
    assert_eq!(
        MachineSetup::host_disk_attach_of(F::LideDrive1),
        Some(HostDiskAttach::LideSlave(0))
    );
    assert_eq!(
        MachineSetup::host_disk_attach_of(F::LideDrive2),
        Some(HostDiskAttach::LideMaster(1))
    );
    assert_eq!(
        MachineSetup::host_disk_attach_of(F::LideDrive3),
        Some(HostDiskAttach::LideSlave(1))
    );
}

/// Mounting a host disk takes over exactly the slot it attaches to: the
/// images on the other slots keep their own `driveN` keys, so there is no
/// gap to avoid and nothing else to clear.
#[test]
fn lide_host_disk_takes_only_the_slot_it_attaches_to() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    s.cycle(F::LideBoard, true); // RIPPLE
    s.set_path(F::LideDrive0, PathBuf::from("ch0-master.hdf"));
    s.set_path(F::LideDrive1, PathBuf::from("ch0-slave.hdf"));
    s.set_path(F::LideDrive2, PathBuf::from("ch1-master.hdf"));

    s.set_host_disks_for_test(vec![HostDiskRow {
        id: "disk4".to_string(),
        fingerprint: None,
        volume: "SanDisk".to_string(),
        size: "31.9 GB".to_string(),
        mounted: Vec::new(),
        writable: true,
        attach: Some(crate::config::HostDiskAttach::LideMaster(0)),
    }]);
    s.select_host_disk(0);
    let mounted = s.mount_host_disks().expect("channel 0 master is fitted");
    assert_eq!(mounted.len(), 1);

    // The host disk took slot 0 and only slot 0.
    assert_eq!(s.path(F::LideDrive0), None, "the host disk owns this slot");
    assert_eq!(s.path(F::LideDrive1), Some(Path::new("ch0-slave.hdf")));
    assert_eq!(s.path(F::LideDrive2), Some(Path::new("ch1-master.hdf")));
    let raw = s.to_raw();
    assert!(raw.lide.drive0.is_none());
    assert_eq!(raw.lide.drive1.as_ref().unwrap().path, "ch0-slave.hdf");
    assert_eq!(raw.lide.drive2.as_ref().unwrap().path, "ch1-master.hdf");
}

/// Cycling the lide board to a personality with fewer channels must drop
/// host disks on channels the new personality no longer has, exactly as
/// it already drops image drives there.
#[test]
fn lide_board_switch_drops_host_disks_on_lost_channels() {
    use crate::config::HostDiskAttach;
    use LauncherField as F;
    let mut s = MachineSetup {
        lide_board: Some(LidePersonality::Ripple), // two channels
        host_disks_attached: vec![crate::config::HostDiskConfig {
            device: "disk4".to_string(),
            fingerprint: None,
            identity_confirmed: true,
            attach: HostDiskAttach::LideSlave(1), // RIPPLE-only channel
            writable: true,
        }],
        host_disk_selected: vec!["disk4".to_string()],
        ..Default::default()
    };
    assert!(s.host_disk_is_attached("disk4"));

    s.cycle(F::LideBoard, true); // RIDE: one channel only
    assert!(
        !s.host_disk_is_attached("disk4"),
        "channel 1 no longer exists on RIDE"
    );
    assert!(s.host_disks_attached().is_empty());
}

#[cfg(feature = "midi")]
#[test]
fn io_ports_pages_carry_one_section_each() {
    let header = |tab| {
        let r = rows(tab, ParallelDevice::None, SerialMode::Midi, false, false);
        r.iter()
            .filter(|x| x.kind == RowKind::SectionHeader)
            .map(|x| x.label)
            .collect::<Vec<_>>()
    };
    // Serial Port page: the Device / Mode selector and (in MIDI) the
    // endpoints, under the one heading.
    assert_eq!(
        header(LauncherTab::IoPorts),
        ["Serial:"],
        "the strip tab is the serial page"
    );
    let r = rows(
        LauncherTab::IoPorts,
        ParallelDevice::None,
        SerialMode::Midi,
        false,
        false,
    );
    assert!(r.iter().any(|x| x.field == LauncherField::SerialMode));
    assert!(r.iter().any(|x| x.field == LauncherField::MidiOut));
    assert!(
        !r.iter().any(|x| x.field == LauncherField::ParallelDevice),
        "parallel lives on its own page now"
    );
    // Parallel Port page: the device selector.
    assert_eq!(header(LauncherTab::IoParallel), ["Parallel:"]);
    let r = rows(
        LauncherTab::IoParallel,
        ParallelDevice::None,
        SerialMode::Midi,
        false,
        false,
    );
    assert!(r.iter().any(|x| x.field == LauncherField::ParallelDevice));
    // Networking page: the A2065 board selector.
    assert_eq!(header(LauncherTab::IoNetworking), ["Ethernet:"]);
    let r = rows(
        LauncherTab::IoNetworking,
        ParallelDevice::None,
        SerialMode::Midi,
        false,
        false,
    );
    assert!(r.iter().any(|x| x.field == LauncherField::Ethernet));
    // Audio page: the Toccata board toggle, and (in an `mhi` build)
    // the MHI board toggle alongside it.
    assert_eq!(header(LauncherTab::IoAudio), ["Sound Card:"]);
    let r = rows(
        LauncherTab::IoAudio,
        ParallelDevice::None,
        SerialMode::Midi,
        false,
        false,
    );
    assert!(r.iter().any(|x| x.field == LauncherField::Toccata));
    #[cfg(feature = "mhi")]
    assert!(r.iter().any(|x| x.field == LauncherField::Mhi));
    // The four pages switch between each other on the nav row, under
    // the one strip entry, with no Back button -- the A/V pattern.
    for tab in [
        LauncherTab::IoParallel,
        LauncherTab::IoNetworking,
        LauncherTab::IoAudio,
    ] {
        assert_eq!(tab.strip_tab(), LauncherTab::IoPorts);
        assert_eq!(tab.parent_tab(), None);
        assert!(tab.has_top_nav());
    }
    assert_eq!(
        LauncherTab::IoPorts.nav_options(),
        [
            ("Serial Port", LauncherTab::IoPorts),
            ("Parallel Port", LauncherTab::IoParallel),
            ("Networking", LauncherTab::IoNetworking),
            ("Audio", LauncherTab::IoAudio),
        ]
    );
}

#[test]
fn a2065_board_cycles_and_round_trips() {
    let mut s = MachineSetup::default();
    assert_eq!(s.value_label(LauncherField::Ethernet), "None");
    assert!(s.to_raw().a2065.net.is_none());
    assert!(!s.ethernet_breaks_determinism());

    s.cycle(LauncherField::Ethernet, true);
    assert_eq!(s.value_label(LauncherField::Ethernet), "Isolated");
    s.cycle(LauncherField::Ethernet, true);
    assert_eq!(s.value_label(LauncherField::Ethernet), "Loopback");
    // Loopback echoes frames on the emulated clock: no determinism warning.
    assert!(!s.ethernet_breaks_determinism());

    // The fitted board and its backend survive a save/load round trip.
    let raw = s.to_raw();
    assert_eq!(raw.a2065.net.as_deref(), Some("loopback"));
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.a2065_net, Some(NetConfig::Loopback));

    if crate::net::NAT_AVAILABLE {
        s.cycle(LauncherField::Ethernet, true);
        assert_eq!(s.value_label(LauncherField::Ethernet), "NAT");
        // NAT carries traffic on the host's schedule.
        assert!(s.ethernet_breaks_determinism());
        assert_eq!(s.to_raw().a2065.net.as_deref(), Some("nat"));
        s.cycle(LauncherField::Ethernet, true);
    } else {
        // Where the NAT cannot come up the picker skips straight past it.
        s.cycle(LauncherField::Ethernet, true);
    }
    assert_eq!(s.value_label(LauncherField::Ethernet), "None");
}

#[test]
fn hostsocket_board_cycles_and_round_trips() {
    let mut s = MachineSetup::default();
    assert_eq!(s.value_label(LauncherField::HostSocket), "None");
    assert!(s.to_raw().hostsocket.net.is_none());
    assert!(!s.ethernet_breaks_determinism());

    s.cycle(LauncherField::HostSocket, true);
    assert_eq!(s.value_label(LauncherField::HostSocket), "Isolated");
    s.cycle(LauncherField::HostSocket, true);
    assert_eq!(s.value_label(LauncherField::HostSocket), "Loopback");
    // HostSocket over loopback is fully deterministic: no warning.
    assert!(!s.ethernet_breaks_determinism());

    // The fitted board and its backend survive a save/load round trip,
    // resolving to the bundled wasm board on the way through Config.
    let raw = s.to_raw();
    assert_eq!(raw.hostsocket.net.as_deref(), Some("loopback"));
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.hostsocket_net, Some(NetConfig::Loopback));

    if crate::net::NAT_AVAILABLE {
        s.cycle(LauncherField::HostSocket, true);
        assert_eq!(s.value_label(LauncherField::HostSocket), "NAT");
        assert!(s.ethernet_breaks_determinism());
        assert_eq!(s.to_raw().hostsocket.net.as_deref(), Some("nat"));
    }
    s.cycle(LauncherField::HostSocket, true);

    // Host: real host OS sockets, not a NetConfig backend at all -- the
    // one choice this board has that A2065's own picker does not (see
    // cycle_hostsocket_board's own comment). Genuinely non-deterministic
    // (real host sockets, same as NAT/bridge), and address/gateway must
    // not survive alongside it -- Config::from_raw rejects both under
    // net = "host".
    assert_eq!(s.value_label(LauncherField::HostSocket), "Host");
    assert!(s.ethernet_breaks_determinism());
    s.hostsocket_address = Some("192.168.1.50/24".to_string());
    s.hostsocket_gateway = Some("192.168.1.1".to_string());
    let raw = s.to_raw();
    assert_eq!(raw.hostsocket.net.as_deref(), Some("host"));
    assert!(raw.hostsocket.interface.is_none());
    assert!(raw.hostsocket.address.is_none());
    assert!(raw.hostsocket.gateway.is_none());
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert!(back.hostsocket_host_mode);
    assert_eq!(back.value_label(LauncherField::HostSocket), "Host");

    s.cycle(LauncherField::HostSocket, true);
    assert_eq!(s.value_label(LauncherField::HostSocket), "None");
    assert!(!s.hostsocket_host_mode);
}

#[test]
fn the_freezer_cartridge_row_cycles_and_round_trips() {
    use crate::cartridge::CartridgeModel;
    let mut s = MachineSetup::default();
    // No cartridge is the baseline, so nothing is written until one is
    // fitted.
    assert_eq!(s.value_label(LauncherField::Cartridge), "None");
    assert!(s.to_raw().cartridge.model.is_none());

    s.cycle(LauncherField::Cartridge, true);
    assert_eq!(s.value_label(LauncherField::Cartridge), "HRTMon");
    let raw = s.to_raw();
    assert_eq!(raw.cartridge.model.as_deref(), Some("hrtmon"));
    assert!(
        raw.cartridge.rom.is_none(),
        "the bundled image needs no rom key"
    );
    let cfg = s.build_config().expect("valid config");
    assert_eq!(cfg.cartridge.model, Some(CartridgeModel::Hrtmon));
    let back = MachineSetup::from_raw(&raw).expect("valid raw");
    assert_eq!(back.value_label(LauncherField::Cartridge), "HRTMon");

    s.cycle(LauncherField::Cartridge, true);
    assert_eq!(s.value_label(LauncherField::Cartridge), "None");
    assert!(s.to_raw().cartridge.model.is_none());

    // An image of the user's own has no row of its own: it rides along
    // while the cartridge stays fitted and goes when it is unfitted.
    let raw: RawConfig =
        toml::from_str("[cartridge]\nmodel = \"hrtmon\"\nrom = \"/roms/hrtmon.rom\"\n").unwrap();
    let mut setup = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(setup.value_label(LauncherField::Cartridge), "HRTMon");
    assert_eq!(
        setup.to_raw().cartridge.rom.as_deref(),
        Some("/roms/hrtmon.rom")
    );
    setup.cycle(LauncherField::Cartridge, true);
    assert!(setup.to_raw().cartridge.model.is_none());
    assert!(setup.to_raw().cartridge.rom.is_none());
}

#[test]
fn toccata_and_mhi_boards_toggle_and_round_trip() {
    let mut s = MachineSetup::default();
    // Off is the baseline for both, so nothing is written until fitted.
    assert!(!s.toggle_value(LauncherField::Toccata));
    #[cfg(feature = "mhi")]
    assert!(!s.toggle_value(LauncherField::Mhi));
    assert_eq!(s.to_raw().toccata.enabled, None);
    #[cfg(feature = "mhi")]
    assert_eq!(s.to_raw().mhi.enabled, None);

    s.cycle(LauncherField::Toccata, true);
    #[cfg(feature = "mhi")]
    s.cycle(LauncherField::Mhi, true);
    assert!(s.toggle_value(LauncherField::Toccata));
    #[cfg(feature = "mhi")]
    assert!(s.toggle_value(LauncherField::Mhi));
    assert_eq!(s.to_raw().toccata.enabled, Some(true));
    #[cfg(feature = "mhi")]
    assert_eq!(s.to_raw().mhi.enabled, Some(true));

    // The written config has to load back into the same settings.
    let cfg = s.build_config().expect("valid config");
    assert!(cfg.toccata);
    #[cfg(feature = "mhi")]
    assert!(cfg.mhi);
    let reloaded = MachineSetup::from_raw(&s.to_raw()).expect("valid raw");
    assert!(reloaded.toggle_value(LauncherField::Toccata));
    #[cfg(feature = "mhi")]
    assert!(reloaded.toggle_value(LauncherField::Mhi));

    s.cycle(LauncherField::Toccata, false);
    #[cfg(feature = "mhi")]
    s.cycle(LauncherField::Mhi, false);
    assert!(!s.toggle_value(LauncherField::Toccata));
    #[cfg(feature = "mhi")]
    assert!(!s.toggle_value(LauncherField::Mhi));
    assert_eq!(s.to_raw().toccata.enabled, None);
    #[cfg(feature = "mhi")]
    assert_eq!(s.to_raw().mhi.enabled, None);
}

#[test]
fn hostsocket_keeps_uneditable_keys_across_a_launcher_save() {
    // dns_server/hostname/address/gateway/resolver have no launcher
    // row; a load-edit-save cycle must carry them through untouched.
    let mut raw = RawConfig::default();
    raw.hostsocket.net = Some("nat".to_string());
    raw.hostsocket.dns_server = Some("192.0.2.53".to_string());
    raw.hostsocket.hostname = Some("boing".to_string());
    raw.hostsocket.address = Some("192.168.1.50/24".to_string());
    raw.hostsocket.gateway = Some("192.168.1.1".to_string());
    raw.hostsocket.resolver = Some("host".to_string());
    let s = MachineSetup::from_raw(&raw).unwrap();
    let saved = s.to_raw();
    assert_eq!(saved.hostsocket.net.as_deref(), Some("nat"));
    assert_eq!(saved.hostsocket.dns_server.as_deref(), Some("192.0.2.53"));
    assert_eq!(saved.hostsocket.hostname.as_deref(), Some("boing"));
    assert_eq!(saved.hostsocket.address.as_deref(), Some("192.168.1.50/24"));
    assert_eq!(saved.hostsocket.gateway.as_deref(), Some("192.168.1.1"));
    assert_eq!(saved.hostsocket.resolver.as_deref(), Some("host"));
}

#[test]
fn a2065_nat_from_a_config_warns_only_where_the_nat_can_come_up() {
    // A loaded config can name NAT even in a build whose picker skips it;
    // there make_backend leaves the NIC isolated (deterministic), so the
    // warning must track NAT_AVAILABLE, not just the selected backend.
    let mut raw = RawConfig::default();
    raw.a2065.net = Some("nat".to_string());
    let s = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(s.value_label(LauncherField::Ethernet), "NAT");
    assert_eq!(s.ethernet_breaks_determinism(), crate::net::NAT_AVAILABLE);
}

#[test]
fn a2065_bridge_interface_picker_round_trips() {
    let mut raw = RawConfig::default();
    raw.a2065.net = Some("bridge".to_string());
    raw.a2065.interface = Some("en-test".to_string());
    let mut setup = MachineSetup::from_raw(&raw).unwrap();
    setup.bridge_interfaces = vec![
        ("en-test".to_string(), "Ethernet A (en-test)".to_string()),
        ("en-next".to_string(), "Ethernet B (en-next)".to_string()),
    ];
    assert_eq!(setup.value_label(F::Ethernet), "Bridged");
    assert_eq!(
        setup.value_label(F::EthernetInterface),
        "Ethernet A (en-test)"
    );
    assert!(!setup.row_hidden(F::EthernetInterface));
    assert_eq!(
        setup.ethernet_breaks_determinism(),
        crate::net::BRIDGE_AVAILABLE
    );

    setup.cycle(F::EthernetInterface, true);
    let saved = setup.to_raw();
    assert_eq!(saved.a2065.net.as_deref(), Some("bridge"));
    assert_eq!(saved.a2065.interface.as_deref(), Some("en-next"));
    let restored = MachineSetup::from_raw(&saved).unwrap();
    assert_eq!(
        restored.a2065_net,
        Some(NetConfig::Bridge {
            interface: "en-next".to_string()
        })
    );
}

#[test]
fn an_isolated_a2065_round_trips_as_net_none() {
    let mut s = MachineSetup::default();
    s.cycle(LauncherField::Ethernet, true);
    let raw = s.to_raw();
    // Fitted-but-isolated is `net = "none"`; not fitted is an absent key.
    assert_eq!(raw.a2065.net.as_deref(), Some("none"));
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.a2065_net, Some(NetConfig::None));
    assert_eq!(back.value_label(LauncherField::Ethernet), "Isolated");
}

#[test]
fn parallel_sampler_rows_appear_only_when_selected() {
    let has = |device| {
        rows(
            LauncherTab::IoParallel,
            device,
            SerialMode::default(),
            false,
            false,
        )
        .iter()
        .any(|r| r.field == LauncherField::SamplerInput)
    };
    // The sampler rows are hidden (not greyed) unless the sampler is chosen.
    assert!(!has(ParallelDevice::None));
    assert!(!has(ParallelDevice::Printer));
    assert!(has(ParallelDevice::Sampler));
}

#[test]
fn midi_rows_appear_only_in_midi_mode() {
    let has = |mode| {
        rows(
            LauncherTab::IoPorts,
            ParallelDevice::None,
            mode,
            false,
            false,
        )
        .iter()
        .any(|r| r.field == LauncherField::MidiOut)
    };
    assert!(!has(SerialMode::Stdout));
    assert!(has(SerialMode::Midi));
}

#[test]
fn parallel_device_cycles_none_printer_sampler() {
    let mut s = MachineSetup::default();
    assert_eq!(s.parallel_device, ParallelDevice::None);
    s.cycle(LauncherField::ParallelDevice, true);
    assert_eq!(s.parallel_device, ParallelDevice::Printer);
    s.cycle(LauncherField::ParallelDevice, true);
    assert_eq!(s.parallel_device, ParallelDevice::Sampler);
    s.cycle(LauncherField::ParallelDevice, true);
    assert_eq!(s.parallel_device, ParallelDevice::None);
}

#[test]
fn parallel_printer_output_row_appears_and_round_trips() {
    let mut s = MachineSetup::default();
    // The Output file row shows only when the printer is selected.
    let has_output = |device| {
        rows(
            LauncherTab::IoParallel,
            device,
            SerialMode::default(),
            false,
            false,
        )
        .iter()
        .any(|r| r.field == LauncherField::ParallelOutput)
    };
    assert!(!has_output(ParallelDevice::None));
    assert!(has_output(ParallelDevice::Printer));

    s.parallel_device = ParallelDevice::Printer;
    // A printer with no capture file yet is not persisted (incomplete).
    assert_eq!(s.to_raw().parallel.device, None);

    s.set_path(LauncherField::ParallelOutput, "captures/out.prn".into());
    let raw = s.to_raw();
    assert_eq!(raw.parallel.device.as_deref(), Some("printer"));
    assert_eq!(raw.parallel.output.as_deref(), Some("captures/out.prn"));
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.parallel_device, ParallelDevice::Printer);
    assert_eq!(
        back.parallel_output.as_deref(),
        Some(std::path::Path::new("captures/out.prn"))
    );
}

#[test]
fn parallel_sampler_selection_round_trips_through_raw() {
    let mut s = MachineSetup {
        parallel_device: ParallelDevice::Sampler,
        sampler_input: Some("BlackHole".into()),
        sampler_gain_db: 6.0,
        ..MachineSetup::default()
    };
    let raw = s.to_raw();
    assert_eq!(raw.parallel.device.as_deref(), Some("sampler"));
    assert_eq!(raw.parallel.sampler_input.as_deref(), Some("BlackHole"));
    assert_eq!(raw.parallel.sampler_gain, Some(6.0));

    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.parallel_device, ParallelDevice::Sampler);
    assert_eq!(back.sampler_input.as_deref(), Some("BlackHole"));
    assert_eq!(back.sampler_gain_db, 6.0);

    // Switching the device to None must still carry the sampler settings
    // through a Save (they do not imply the sampler on reload).
    s.parallel_device = ParallelDevice::None;
    let raw = s.to_raw();
    assert_eq!(raw.parallel.device, None);
    assert_eq!(raw.parallel.sampler_input.as_deref(), Some("BlackHole"));
    assert_eq!(raw.parallel.sampler_gain, Some(6.0));
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.parallel_device, ParallelDevice::None);
    assert_eq!(back.sampler_input.as_deref(), Some("BlackHole"));
    assert_eq!(back.sampler_gain_db, 6.0);
}

#[test]
fn joystick_input_mode_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    // Default is Gamepad, which emits no [input] section.
    assert_eq!(s.joystick_input_mode, JoystickInputMode::Gamepad);
    assert!(s.to_raw().input.joystick.is_none());
    // The stepper flips between the two explicit modes.
    s.cycle(LauncherField::Joystick, true);
    assert_eq!(s.joystick_input_mode, JoystickInputMode::Keyboard);
    let raw = s.to_raw();
    assert_eq!(raw.input.joystick.as_deref(), Some("keyboard"));
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.joystick_input_mode, JoystickInputMode::Keyboard);
    s.cycle(LauncherField::Joystick, true);
    assert_eq!(s.joystick_input_mode, JoystickInputMode::Gamepad);
    // Switching machine profile resets it to the Gamepad default.
    let mut s = MachineSetup::default();
    s.cycle(LauncherField::Joystick, true);
    s.select_model(Some(MachineModel::A1200));
    assert_eq!(s.joystick_input_mode, JoystickInputMode::Gamepad);
}

#[test]
fn input_routing_summary_names_the_driving_source_per_port() {
    let mut s = MachineSetup::default();
    // Stock wiring, gamepad mode.
    let lines = s.input_routing_summary();
    assert!(lines[0].contains("host mouse"), "{lines:?}");
    assert!(lines[1].contains("gamepad"), "{lines:?}");

    // Stock wiring, keyboard mode: the cursor keys take the joystick.
    s.cycle(LauncherField::Joystick, true);
    let lines = s.input_routing_summary();
    assert!(lines[1].contains("cursor keys"), "{lines:?}");

    // Two joysticks: the numpad stand-in is called out.
    s.port_devices = [PortDevice::Joystick, PortDevice::Joystick];
    let lines = s.input_routing_summary();
    assert!(lines.iter().any(|l| l.contains("numpad")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("cursor keys")), "{lines:?}");

    // Two mice, keyboard mode: the second mouse is keyboard-driven.
    s.port_devices = [PortDevice::Mouse, PortDevice::Mouse];
    let lines = s.input_routing_summary();
    assert!(lines[0].contains("host mouse"), "{lines:?}");
    assert!(lines[1].contains("as a mouse"), "{lines:?}");

    // Two mice, gamepad mode: the second mouse is undriven, with the
    // remedy named.
    s.cycle(LauncherField::Joystick, true);
    let lines = s.input_routing_summary();
    assert!(
        lines[1].contains("flip Joystick input to keyboard"),
        "{lines:?}"
    );

    // Analogue and empty ports say how (or that nothing) drives them.
    s.port_devices = [PortDevice::Analogue, PortDevice::None];
    let lines = s.input_routing_summary();
    assert!(lines[0].contains("pot-after"), "{lines:?}");
    assert!(lines[1].contains("empty"), "{lines:?}");

    // Every device/mode combination fits the settings pane (the panel
    // draws these at 8 px per character with no wrapping).
    let all = [
        PortDevice::Mouse,
        PortDevice::Joystick,
        PortDevice::Cd32Pad,
        PortDevice::Analogue,
        PortDevice::None,
    ];
    for p0 in all {
        for p1 in all {
            for _ in 0..2 {
                s.cycle(LauncherField::Joystick, true);
                s.port_devices = [p0, p1];
                for line in s.input_routing_summary() {
                    assert!(
                        line.chars().count() <= 68,
                        "summary line too wide for the pane: {line:?}"
                    );
                }
            }
        }
    }
}

/// Motherboard RAM tracks both of its hardware constraints: the big-box
/// model gate and the CPU's address reach. Downgrading the CPU to a
/// 24-bit part drops the profile-default bank (not just the row's
/// editability) so the emitted config still validates, and the greyed
/// row names whichever constraint bit.
#[test]
fn motherboard_ram_follows_model_and_cpu_gates() {
    let mut s = MachineSetup::default();
    assert_eq!(
        s.disabled_reason(LauncherField::MbRam),
        Some("needs A3000/A4000")
    );
    s.select_model(Some(MachineModel::A3000));
    assert!(s.applies(LauncherField::MbRam));
    assert_eq!(s.mb_ram, 4 * 1024 * 1024);
    while s.cpu != CpuModel::M68000 {
        s.cycle(LauncherField::Cpu, true);
    }
    assert_eq!(s.mb_ram, 0);
    assert_eq!(
        s.disabled_reason(LauncherField::MbRam),
        Some("needs 32-bit CPU")
    );
    // The profile's Zorro III RTG card is beyond a 24-bit bus too. The RTG
    // selector remains live because Picasso II/II+ are still valid choices.
    assert_eq!(s.rtg, RtgCard::None);
    assert_eq!(s.disabled_reason(LauncherField::Rtg), None);
    // The raw config overrides the profile default back to zero, so
    // this machine still launches.
    assert_eq!(s.to_raw().memory.motherboard.as_deref(), Some("0"));
    s.build_config()
        .expect("68000 A3000 with no mb RAM validates");
}

/// Only the A4000 cycles past Ramsey's 16M four-bank maximum into the
/// $04000000-$06FFFFFF expansion presets; the A3000 wraps back to zero.
#[test]
fn motherboard_ram_expansion_presets_are_a4000_only() {
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A4000));
    while s.mb_ram != 16 * 1024 * 1024 {
        s.cycle(LauncherField::MbRam, true);
    }
    s.cycle(LauncherField::MbRam, true);
    assert_eq!(s.mb_ram, 32 * 1024 * 1024);
    s.cycle(LauncherField::MbRam, true);
    assert_eq!(s.mb_ram, 64 * 1024 * 1024);
    s.build_config().expect("64M A4000 motherboard validates");

    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A3000));
    while s.mb_ram != 16 * 1024 * 1024 {
        s.cycle(LauncherField::MbRam, true);
    }
    s.cycle(LauncherField::MbRam, true);
    assert_eq!(s.mb_ram, 0);
}

/// Accelerator RAM follows only the CPU's address reach: any 32-bit
/// machine can carry it, and downgrading the CPU to a 24-bit part drops
/// the bank so the emitted config still validates.
#[test]
fn accelerator_ram_follows_the_cpu_gate() {
    let mut s = MachineSetup::default();
    // The default machine is a 68000 A500: greyed out.
    assert_eq!(
        s.disabled_reason(LauncherField::AccelRam),
        Some("needs 32-bit CPU")
    );
    s.select_model(Some(MachineModel::A1200));
    while !cpu_is_32bit(s.cpu) {
        s.cycle(LauncherField::Cpu, true);
    }
    assert!(s.applies(LauncherField::AccelRam));
    s.cycle(LauncherField::AccelRam, true);
    assert_eq!(s.accel_ram, 16 * 1024 * 1024);
    assert_eq!(s.to_raw().memory.accelerator.as_deref(), Some("16M"));
    s.build_config()
        .expect("32-bit A1200 with accelerator RAM validates");
    while s.cpu != CpuModel::M68EC020 {
        s.cycle(LauncherField::Cpu, true);
    }
    assert_eq!(s.accel_ram, 0);
    assert_eq!(
        s.disabled_reason(LauncherField::AccelRam),
        Some("needs 32-bit CPU")
    );
}

#[test]
fn port_devices_round_trip_through_raw_against_the_profile_baseline() {
    let mut s = MachineSetup::default();
    // Stock wiring emits no port keys.
    assert_eq!(s.port_devices, [PortDevice::Mouse, PortDevice::Joystick]);
    let raw = s.to_raw();
    assert!(raw.input.port1.is_none());
    assert!(raw.input.port2.is_none());

    // Non-default devices are written and read back. Port 1 offers the
    // gamepad-driven mouse between Mouse and Joystick; port 2 does not
    // offer it at all, a mouse belonging in port 1.
    s.cycle(LauncherField::Port1Device, true); // Mouse -> Gamepad Mouse
    assert_eq!(s.port_devices[0], PortDevice::GamepadMouse);
    assert_eq!(s.to_raw().input.port1.as_deref(), Some("gamepad-mouse"));
    s.cycle(LauncherField::Port1Device, true); // -> Joystick
    s.cycle(LauncherField::Port2Device, true); // Joystick -> Cd32Pad
    let raw = s.to_raw();
    assert_eq!(raw.input.port1.as_deref(), Some("joystick"));
    assert_eq!(raw.input.port2.as_deref(), Some("cd32"));
    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(
        back.port_devices,
        [PortDevice::Joystick, PortDevice::Cd32Pad]
    );

    // The CD32 profile's bundled pad is its baseline: selecting the
    // model adopts it and keeps it implicit in the raw config.
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::Cd32));
    assert_eq!(s.port_devices[1], PortDevice::Cd32Pad);
    assert!(s.to_raw().input.port2.is_none());
}

#[test]
fn build_config_surfaces_validation_errors() {
    // Z3 RAM on a 68000 (24-bit bus) is rejected by the config validator;
    // the model leans on that rather than re-checking.
    let mut s = MachineSetup::default();
    s.cycle(LauncherField::Z3Ram, true);
    assert_eq!(s.z3_ram, 16 * 1024 * 1024);
    let err = s.build_config().unwrap_err().to_string();
    assert!(err.contains("Zorro III"), "{err}");
}

#[cfg(feature = "midi")]
#[test]
fn serial_midi_settings_round_trip_through_raw() {
    let mut s = MachineSetup::default();
    // Default serial mode writes nothing.
    assert!(s.to_raw().serial.mode.is_none());

    s.cycle(LauncherField::SerialMode, true); // Stdout -> MIDI
    assert_eq!(s.serial_mode, SerialMode::Midi);
    s.midi_out = Some("USB MIDI".to_string());

    let raw = s.to_raw();
    assert_eq!(raw.serial.mode.as_deref(), Some("midi"));
    assert_eq!(raw.serial.midi_out.as_deref(), Some("USB MIDI"));

    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.serial_mode, SerialMode::Midi);
    assert_eq!(back.midi_out.as_deref(), Some("USB MIDI"));
}

#[test]
fn shader_cycles_the_presets_and_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    // Off is the baseline, so nothing is written for it.
    assert_eq!(s.value_label(LauncherField::Shader), "Disabled");
    assert_eq!(s.to_raw().display.shader, None);

    // With no user shader configured the picker offers the presets only,
    // and wraps straight back to Off.
    for expected in ["Scanlines", "Mask", "CRT (1084)", "Disabled"] {
        s.cycle(LauncherField::Shader, true);
        assert_eq!(s.value_label(LauncherField::Shader), expected);
    }
    // Backwards from Off lands on the last preset, not on Custom.
    s.cycle(LauncherField::Shader, false);
    assert_eq!(s.shader, ShaderMode::Crt);

    s.cycle(LauncherField::Shader, true);
    s.cycle(LauncherField::Shader, true);
    assert_eq!(s.shader, ShaderMode::Scanlines);
    // The canonical name is written, not the menu's "off" spelling.
    let raw = s.to_raw();
    assert_eq!(raw.display.shader.as_deref(), Some("scanlines"));
    assert_eq!(
        MachineSetup::from_raw(&raw).unwrap().shader,
        ShaderMode::Scanlines
    );
    assert_eq!(
        s.build_config().expect("valid config").shader,
        ShaderMode::Scanlines
    );

    // Switching machine profile returns it to the profile default.
    s.select_model(Some(MachineModel::A1200));
    assert_eq!(s.shader, ShaderMode::None);
}

#[test]
fn a_configured_user_shader_stays_in_the_shader_cycle() {
    let raw = RawConfig {
        display: crate::config::RawDisplay {
            shader: Some("shaders/Aperture.wgsl".to_string()),
            ..Default::default()
        },
        ..RawConfig::default()
    };
    let mut s = MachineSetup::from_raw(&raw).unwrap();
    let custom = ShaderMode::Custom(PathBuf::from("shaders/Aperture.wgsl"));
    assert_eq!(s.shader, custom);
    assert_eq!(s.value_label(LauncherField::Shader), "Custom");
    // The path is written back verbatim, since host paths are
    // case-sensitive.
    assert_eq!(
        s.to_raw().display.shader.as_deref(),
        Some("shaders/Aperture.wgsl")
    );

    // Custom joins the cycle after the last preset, and cycling away and
    // back keeps its path.
    s.cycle(LauncherField::Shader, true);
    assert_eq!(s.shader, ShaderMode::None);
    for _ in 0..4 {
        s.cycle(LauncherField::Shader, true);
    }
    assert_eq!(s.shader, custom);

    // Selecting a preset drops the custom shader from the config file,
    // but not from the picker.
    s.cycle(LauncherField::Shader, true);
    s.cycle(LauncherField::Shader, true);
    assert_eq!(s.shader, ShaderMode::Scanlines);
    assert_eq!(s.to_raw().display.shader.as_deref(), Some("scanlines"));
    s.cycle(LauncherField::Shader, false);
    assert_eq!(s.shader, ShaderMode::None);
    s.cycle(LauncherField::Shader, false);
    assert_eq!(s.shader, custom);
}

/// The shader name is spelled out in six places (the parser, the picker
/// labels, this writer, the menu label, and the two docs tables), so pin
/// the one that matters: whatever `to_raw` writes has to load back as
/// the same mode.
#[test]
fn every_shader_name_parses_back_to_its_own_mode() {
    for mode in [
        ShaderMode::None,
        ShaderMode::Scanlines,
        ShaderMode::Mask,
        ShaderMode::Crt,
        ShaderMode::Custom(PathBuf::from("shaders/Aperture.wgsl")),
    ] {
        let name = shader_name(&mode);
        assert_eq!(
            crate::config::parse_shader(&name).expect("shader name must parse"),
            mode,
            "shader_name({mode:?}) wrote {name:?}, which does not load back"
        );
    }
}

#[test]
fn shader_strength_steps_in_tenths_and_clamps() {
    let mut s = MachineSetup::default();
    // Full effect is the baseline, so nothing is written for it.
    assert_eq!(s.value_label(LauncherField::ShaderStrength), "1.00");
    assert_eq!(s.to_raw().display.shader_strength, None);

    s.cycle(LauncherField::ShaderStrength, false);
    assert_eq!(s.value_label(LauncherField::ShaderStrength), "0.90");
    assert_eq!(s.to_raw().display.shader_strength, Some(0.9));

    // Both ends saturate rather than wrapping, and stepping stays on the
    // 0.1 grid instead of drifting.
    for _ in 0..20 {
        s.cycle(LauncherField::ShaderStrength, false);
    }
    assert_eq!(s.shader_strength, 0.0);
    for _ in 0..20 {
        s.cycle(LauncherField::ShaderStrength, true);
    }
    assert_eq!(s.shader_strength, 1.0);
    assert_eq!(s.to_raw().display.shader_strength, None);

    s.cycle(LauncherField::ShaderStrength, false);
    assert_eq!(s.build_config().expect("valid config").shader_strength, 0.9);
    // Switching machine profile returns it to the profile default.
    s.select_model(Some(MachineModel::A1200));
    assert_eq!(s.shader_strength, 1.0);
}

#[test]
fn stereo_separation_cycles_up_on_right_and_greys_out_in_mono() {
    let mut s = MachineSetup::default();
    assert_eq!(s.audio_stereo_separation, 100);
    assert_eq!(
        s.disabled_reason(LauncherField::AudioStereoSeparation),
        None
    );

    // Right arrow (forward) steps up in 10s, wrapping 100 -> 0 -> 10.
    s.cycle(LauncherField::AudioStereoSeparation, true);
    assert_eq!(s.audio_stereo_separation, 0);
    s.cycle(LauncherField::AudioStereoSeparation, true);
    assert_eq!(s.audio_stereo_separation, 10);

    // Left arrow (backward) from 100 steps down to 90.
    let mut s = MachineSetup::default();
    s.cycle(LauncherField::AudioStereoSeparation, false);
    assert_eq!(s.audio_stereo_separation, 90);

    // Once the output is mono, separation is greyed out.
    s.cycle(LauncherField::AudioChannelMode, true);
    assert_eq!(s.audio_channel_mode, ChannelMode::Mono);
    assert_eq!(
        s.disabled_reason(LauncherField::AudioStereoSeparation),
        Some("mono")
    );
}

#[test]
fn display_start_toggles_round_trip_through_raw_config() {
    let mut s = MachineSetup::default();
    // Defaults (windowed, status bar shown) emit nothing.
    let raw = s.to_raw();
    assert_eq!(raw.display.full_screen, None);
    assert_eq!(raw.display.status_bar, None);
    assert!(!s.toggle_value(LauncherField::StartFullscreen));
    assert!(s.toggle_value(LauncherField::ShowStatusBar));

    // Flip both; the non-default values now persist to [display].
    s.cycle(LauncherField::StartFullscreen, true);
    s.cycle(LauncherField::ShowStatusBar, true);
    let raw = s.to_raw();
    assert_eq!(raw.display.full_screen, Some(true));
    assert_eq!(raw.display.status_bar, Some(false));
}

#[test]
fn disabled_audio_greys_out_channel_mode_and_separation() {
    use crate::audio::AudioOutput;
    let mut s = MachineSetup::default();
    // Enabled: channel mode, filter, and separation are all active.
    assert_eq!(s.disabled_reason(LauncherField::AudioChannelMode), None);
    assert_eq!(s.disabled_reason(LauncherField::AudioFilter), None);
    assert_eq!(
        s.disabled_reason(LauncherField::AudioStereoSeparation),
        None
    );

    // Disabled audio greys the shaping controls.
    s.audio_output = AudioOutput::Disabled;
    assert_eq!(
        s.disabled_reason(LauncherField::AudioChannelMode),
        Some("off")
    );
    assert_eq!(s.disabled_reason(LauncherField::AudioFilter), Some("off"));
    assert_eq!(
        s.disabled_reason(LauncherField::AudioStereoSeparation),
        Some("off")
    );
}

#[test]
fn audio_output_disabled_round_trips_through_raw_config() {
    use crate::audio::AudioOutput;
    let mut s = MachineSetup::default();
    // Default is the resolved default, so it emits nothing.
    assert_eq!(s.value_label(LauncherField::AudioDevice), "Default");
    let raw = s.to_raw();
    assert_eq!(raw.audio.output_enabled, None);
    assert_eq!(raw.audio.output_device, None);

    // "Disabled" persists as output_enabled = false, no device.
    s.audio_output = AudioOutput::Disabled;
    assert_eq!(s.value_label(LauncherField::AudioDevice), "Disabled");
    let raw = s.to_raw();
    assert_eq!(raw.audio.output_enabled, Some(false));
    assert_eq!(raw.audio.output_device, None);

    // A named device persists as output_device, with output_enabled omitted.
    s.audio_output = AudioOutput::Device("BlackHole".to_string());
    let raw = s.to_raw();
    assert_eq!(raw.audio.output_device.as_deref(), Some("BlackHole"));
    assert_eq!(raw.audio.output_enabled, None);
}

#[test]
fn stem_granularity_survives_a_launcher_load_and_save() {
    // No launcher row edits [audio] stem_granularity (it is a
    // headless-only default), so a loaded config's value must pass
    // through to_raw untouched rather than being dropped on Save.
    let mut raw = RawConfig::default();
    raw.audio.stem_granularity = Some("master,channel".to_string());
    let setup = MachineSetup::from_raw(&raw).expect("config loads");
    assert_eq!(
        setup.to_raw().audio.stem_granularity.as_deref(),
        Some("master,channel")
    );
    // And a config that never set it keeps not setting it.
    let bare = MachineSetup::from_raw(&RawConfig::default()).expect("config loads");
    assert_eq!(bare.to_raw().audio.stem_granularity, None);
}

#[cfg(feature = "midi")]
#[test]
fn midi_device_rows_are_hidden_off_midi_mode() {
    // Off MIDI mode the endpoint rows are absent from the Serial section
    // (they are hidden, not greyed), so it shows only the Device / Mode row.
    let serial = rows(
        LauncherTab::IoPorts,
        ParallelDevice::None,
        SerialMode::Stdout,
        false,
        false,
    );
    assert!(!serial.iter().any(|r| r.field == LauncherField::MidiOut));
    assert!(!serial.iter().any(|r| r.field == LauncherField::MidiIn));
}

#[test]
fn setting_a_floppy_path_round_trips_and_wires_the_drive() {
    let mut s = MachineSetup::default();
    s.set_path(LauncherField::Df1Image, PathBuf::from("/disks/b.adf"));
    assert!(s.floppy_drives >= 2, "DF1 media wires in a second drive");
    let raw = s.to_raw();
    assert_eq!(raw.floppy.drives, Some(2));
    assert_eq!(
        raw.floppy.df1.as_ref().and_then(|d| d.path.as_deref()),
        Some("/disks/b.adf")
    );
}

#[test]
fn drive_volume_name_round_trips_through_raw() {
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A1200)); // Gayle, so IDE applies.
    s.set_path(LauncherField::IdeMaster, PathBuf::from("/host/games"));
    s.set_drive_name(LauncherField::IdeMaster, "Games".to_string());
    assert_eq!(s.drive_name(LauncherField::IdeMaster), Some("Games"));

    let raw = s.to_raw();
    let master = raw.ide.master.as_ref().expect("master emitted");
    assert_eq!(master.path, "/host/games");
    assert_eq!(master.name.as_deref(), Some("Games"));

    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(back.drive_name(LauncherField::IdeMaster), Some("Games"));
}

#[test]
fn drive_volume_name_without_an_image_is_dropped() {
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A1200));
    // No image set: a name has nothing to label.
    s.set_drive_name(LauncherField::IdeMaster, "Orphan".to_string());
    assert_eq!(s.drive_name(LauncherField::IdeMaster), None);

    // With an image the name sticks, then clearing the image drops it too.
    s.set_path(LauncherField::IdeMaster, PathBuf::from("/host/games"));
    s.set_drive_name(LauncherField::IdeMaster, "Games".to_string());
    assert_eq!(s.drive_name(LauncherField::IdeMaster), Some("Games"));
    s.clear_path(LauncherField::IdeMaster);
    assert_eq!(s.drive_name(LauncherField::IdeMaster), None);
}

#[test]
fn drive_filesystem_round_trips_through_raw_for_a_directory_mount() {
    let dir = std::env::temp_dir().join(format!(
        "copperline-launcher-fs-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A1200)); // Gayle, so IDE applies.
    s.set_path(LauncherField::IdeMaster, dir.clone());
    // FFS by default, and an FFS default is not written out at all.
    assert_eq!(
        s.drive_filesystem(LauncherField::IdeMaster),
        crate::diskimage::FileSystem::FFS
    );
    let raw = s.to_raw();
    assert_eq!(raw.ide.master.as_ref().unwrap().filesystem, None);

    s.cycle_drive_filesystem(LauncherField::IdeMaster);
    assert_eq!(
        s.drive_filesystem(LauncherField::IdeMaster),
        crate::diskimage::FileSystem::OFS
    );
    let raw = s.to_raw();
    assert_eq!(
        raw.ide.master.as_ref().unwrap().filesystem.as_deref(),
        Some("ofs")
    );

    let back = MachineSetup::from_raw(&raw).unwrap();
    assert_eq!(
        back.drive_filesystem(LauncherField::IdeMaster),
        crate::diskimage::FileSystem::OFS
    );

    // Toggling back to FFS and clearing the path both drop it again.
    s.cycle_drive_filesystem(LauncherField::IdeMaster);
    assert_eq!(
        s.to_raw().ide.master.as_ref().unwrap().filesystem,
        None,
        "FFS is the default and is not written out"
    );
    s.cycle_drive_filesystem(LauncherField::IdeMaster);
    s.clear_path(LauncherField::IdeMaster);
    assert_eq!(
        s.drive_filesystem(LauncherField::IdeMaster),
        crate::diskimage::FileSystem::FFS,
        "clearing the image resets the filesystem choice, like the volume name"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn drive_filesystem_is_not_emitted_for_a_non_directory_path() {
    // A raw HDF/image path already carries its own filesystem; even if
    // the field is toggled (e.g. leftover session state from an earlier
    // directory at the same slot), `to_raw` must never write a
    // `filesystem` key config.rs would then reject at parse time.
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A1200));
    s.set_path(LauncherField::IdeMaster, PathBuf::from("/host/games.hdf"));
    s.cycle_drive_filesystem(LauncherField::IdeMaster);
    assert_eq!(
        s.drive_filesystem(LauncherField::IdeMaster),
        crate::diskimage::FileSystem::OFS
    );
    let raw = s.to_raw();
    assert_eq!(raw.ide.master.as_ref().unwrap().filesystem, None);
}

#[test]
fn editing_a_drive_name_commits_to_the_setup() {
    let mut setup = MachineSetup::default();
    setup.select_model(Some(MachineModel::A1200));
    setup.set_path(LauncherField::ScsiUnit0, PathBuf::from("/host/work"));
    let mut state = LauncherState::new(setup);
    state.begin_edit_drive_name(LauncherField::ScsiUnit0);
    for ch in "WORK".chars() {
        state.edit_push(ch);
    }
    state.edit_commit();
    assert_eq!(state.editing(), None);
    assert_eq!(
        state.setup.drive_name(LauncherField::ScsiUnit0),
        Some("WORK")
    );
}

/// Every drive that can boot lands on the Boot Priority page, and the Lide
/// board's drives land after the machine's own -- which is also the order
/// the cascade default ranks them in.
#[test]
fn lide_drives_take_their_place_after_ide_and_scsi_in_the_boot_order() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A1200));
    s.set_path(F::IdeMaster, PathBuf::from("ide0.hdf"));
    s.cycle(F::LideBoard, true); // RIPPLE: two channels, four drives
    s.set_path(F::LideDrive0, PathBuf::from("lide0.hdf"));

    let listed: Vec<_> = rows(
        LauncherTab::BootPriority,
        Default::default(),
        Default::default(),
        false,
        false,
    )
    .iter()
    .filter(|r| s.row_on_page(LauncherTab::BootPriority, r.field))
    .map(|r| r.field)
    .collect();
    // The column titles, both IDE bays (the empty one stands its ground),
    // then the one filled Lide slot. The empty Lide slots take no place.
    assert_eq!(
        listed,
        vec![
            F::SectionHeader,
            F::IdeMasterBoot,
            F::IdeSlaveBoot,
            F::LideDrive0Boot
        ]
    );
    assert_eq!(s.boot_priority_row_count(), 3);
}

/// The boot order runs onto a second page only when it outgrows the first,
/// and a drive's page follows its position among the drives actually listed
/// -- not its position in the table.
#[test]
fn a_long_boot_order_pages_and_a_short_one_does_not() {
    use LauncherField as F;
    let mut s = MachineSetup::default();
    s.select_model(Some(MachineModel::A1200));
    s.cycle(F::LideBoard, true); // RIPPLE
    for f in [F::LideDrive0, F::LideDrive1, F::LideDrive2, F::LideDrive3] {
        s.set_path(f, PathBuf::from("lide.hdf"));
    }

    // Two IDE bays and four Lide drives: six rows, one page, and the note
    // still has room under them. No SCSI in between, so every Lide drive
    // stays on the first page -- paging counts drives listed, not table
    // positions.
    assert_eq!(s.boot_priority_row_count(), 6);
    assert!(!s.boot_priority_has_second_page());
    assert_eq!(
        s.boot_page_of(F::LideDrive3Boot),
        Some(LauncherTab::BootPriority)
    );

    // Fill the SCSI chain as well and the order outgrows one page. The
    // first nine drives stay; the rest move over, and the note goes to
    // make room for them.
    s.cycle(F::ScsiController, true);
    for f in [
        F::ScsiUnit0,
        F::ScsiUnit1,
        F::ScsiUnit2,
        F::ScsiUnit3,
        F::ScsiUnit4,
        F::ScsiUnit5,
        F::ScsiUnit6,
    ] {
        s.set_path(f, PathBuf::from("scsi.hdf"));
    }
    assert_eq!(s.boot_priority_row_count(), 13);
    assert!(s.boot_priority_has_second_page());

    // Two IDE bays and seven SCSI units fill the first page exactly; all
    // four Lide drives fall onto the second.
    for f in [F::IdeMasterBoot, F::IdeSlaveBoot, F::ScsiUnit6Boot] {
        assert_eq!(s.boot_page_of(f), Some(LauncherTab::BootPriority), "{f:?}");
    }
    for f in [
        F::LideDrive0Boot,
        F::LideDrive1Boot,
        F::LideDrive2Boot,
        F::LideDrive3Boot,
    ] {
        assert_eq!(
            s.boot_page_of(f),
            Some(LauncherTab::BootPriorityMore(2)),
            "{f:?}"
        );
    }
    // Neither page draws more rows than one holds.
    for tab in [LauncherTab::BootPriority, LauncherTab::BootPriorityMore(2)] {
        let drawn = rows(tab, Default::default(), Default::default(), false, false)
            .iter()
            .filter(|r| s.row_on_page(tab, r.field))
            .count();
        assert!(drawn <= BOOTPRI_PAGE_ROWS + 1, "{tab:?} draws {drawn}");
    }
    assert_eq!(s.boot_priority_page_count(), 2);

    // Fill every copperhf unit too: 20 rows, and the pagination grows a
    // third page rather than silently stranding the overflow -- the exact
    // capacity bug the fixed two-page implementation had when copperhf's
    // seven rows first landed.
    for f in [
        F::CopperhfUnit0,
        F::CopperhfUnit1,
        F::CopperhfUnit2,
        F::CopperhfUnit3,
        F::CopperhfUnit4,
        F::CopperhfUnit5,
        F::CopperhfUnit6,
    ] {
        s.set_path(f, PathBuf::from("copperhf.hdf"));
    }
    assert_eq!(s.boot_priority_row_count(), 20);
    assert_eq!(s.boot_priority_page_count(), 3);

    // With every slot filled, each page draws exactly its share -- the same
    // filter the draw path uses, not a re-stated cap -- and every listed
    // row is reachable on exactly one page.
    let pages = [
        LauncherTab::BootPriority,
        LauncherTab::BootPriorityMore(2),
        LauncherTab::BootPriorityMore(3),
    ];
    let per_page: Vec<usize> = pages
        .iter()
        .map(|&tab| {
            BOOTPRI_ROWS
                .iter()
                .filter(|r| s.row_on_page(tab, r.field))
                .count()
        })
        .collect();
    assert_eq!(per_page, vec![BOOTPRI_PAGE_ROWS, BOOTPRI_PAGE_ROWS, 2]);
    for row in BOOTPRI_ROWS.iter() {
        let on = pages
            .iter()
            .filter(|&&tab| s.row_on_page(tab, row.field))
            .count();
        assert_eq!(on, 1, "{:?} reachable on exactly one page", row.field);
    }

    // "Next Page >" walks forward through every page and stops at the last.
    assert_eq!(
        s.boot_priority_next_page(LauncherTab::BootPriority),
        Some(LauncherTab::BootPriorityMore(2))
    );
    assert_eq!(
        s.boot_priority_next_page(LauncherTab::BootPriorityMore(2)),
        Some(LauncherTab::BootPriorityMore(3))
    );
    assert_eq!(
        s.boot_priority_next_page(LauncherTab::BootPriorityMore(3)),
        None
    );

    // A setup swapped in under the panel (Load..., Defaults) may not have
    // the page the session was standing on; a vanished page settles back to
    // the first.
    assert_eq!(
        s.settle_tab(LauncherTab::BootPriorityMore(3)),
        LauncherTab::BootPriorityMore(3),
        "a full machine keeps its third page"
    );
    let empty = MachineSetup::default();
    assert_eq!(
        empty.settle_tab(LauncherTab::BootPriorityMore(2)),
        LauncherTab::BootPriority
    );
    assert_eq!(empty.settle_tab(LauncherTab::Lide), LauncherTab::Lide);

    // Back on a later page returns to the first, not to Storage.
    assert_eq!(
        LauncherTab::BootPriorityMore(2).parent_tab(),
        Some(LauncherTab::BootPriority)
    );
    assert_eq!(
        LauncherTab::BootPriority.parent_tab(),
        Some(LauncherTab::Storage)
    );
}
#[test]
fn netplay_setup_edits_all_controls_without_persisting_connection_details() -> Result<()> {
    let mut state = LauncherState::new(MachineSetup::default());
    assert!(!state.row_toggle(F::NetplayEnabled));
    assert!(!state.row_applies(F::NetplayPeer));
    state.toggle_netplay();
    assert!(state.row_toggle(F::NetplayEnabled));
    assert_eq!(
        state.setup.port_devices,
        [PortDevice::Mouse, PortDevice::Joystick]
    );
    assert_eq!(state.setup.serial_mode, SerialMode::Off);
    for (field, value) in [
        (F::NetplayBind, "[::]:19732"),
        (F::NetplayPeer, "[::1]:19733"),
        (F::NetplayCode, "0123456789ABCDEF0123456789abcdef"),
    ] {
        state.begin_edit_netplay(field);
        state.clear_netplay_edit();
        for c in value.chars() {
            state.edit_push(c);
        }
        state.edit_commit();
        assert_eq!(state.row_value(field), value);
        assert!(state.row_applies(field));
    }
    for (field, count) in [
        (F::NetplayPlayer, 2),
        (F::NetplayDelay, 7),
        (F::NetplayRollback, 12),
    ] {
        let initial = state.row_value(field);
        let mut values = std::collections::BTreeSet::new();
        for _ in 0..count {
            values.insert(state.row_value(field));
            state.netplay.cycle(field, true);
        }
        assert_eq!(values.len(), count);
        assert_eq!(state.row_value(field), initial);
        state.netplay.cycle(field, false);
        state.netplay.cycle(field, true);
        assert_eq!(state.row_value(field), initial);
    }
    let options = state.netplay.options()?.unwrap();
    assert!(options.bind.is_ipv6());
    assert!(options.peer.is_ipv6());
    assert!(TABS.contains(&LauncherTab::Netplay));
    assert!(tabs(true).contains(&LauncherTab::Netplay));
    let raw = state.setup.to_raw();
    let reloaded = LauncherState::from_raw(&raw);
    assert!(!reloaded.netplay.enabled);
    assert!(reloaded.netplay.code.is_empty());
    assert!(reloaded.netplay.peer.is_empty());
    Ok(())
}

#[test]
fn netplay_setup_rejects_invalid_details_and_generates_fresh_codes() {
    let mut setup = NetplaySetup {
        enabled: true,
        ..Default::default()
    };
    assert!(setup.options().is_err());
    setup.peer = "127.0.0.1:19733".into();
    setup.new_code();
    let first = setup.code.clone();
    assert!(setup.options().is_ok());
    setup.new_code();
    assert_ne!(setup.code, first);
    assert!(setup.options().is_ok());
    for invalid in ["", "xyz", "éééééééééééééééé"] {
        setup.code = invalid.into();
        assert!(setup.options().is_err());
    }
    setup.enabled = false;
    assert!(setup.options().unwrap().is_none());
}

#[test]
fn netplay_preserves_each_supported_port_device() {
    let mut state = LauncherState::new(MachineSetup::default());
    state.netplay.enabled = true;
    for device in [
        PortDevice::Mouse,
        PortDevice::Joystick,
        PortDevice::Cd32Pad,
        PortDevice::Analogue,
        PortDevice::GamepadMouse,
        PortDevice::None,
    ] {
        state.setup.port_devices = [device; 2];
        state.prepare_netplay_machine();
        let expected = match device {
            PortDevice::Mouse | PortDevice::Joystick | PortDevice::Cd32Pad => device,
            _ => PortDevice::Joystick,
        };
        assert_eq!(state.setup.port_devices, [expected; 2]);
    }
}

#[cfg(feature = "netplay-internet")]
#[test]
fn internet_netplay_launcher_shares_invitation_and_adopts_host_timing() -> Result<()> {
    let mut host = LauncherState::new(MachineSetup::default());
    host.toggle_netplay();
    host.tab = LauncherTab::Netplay;
    host.netplay.cycle(F::NetplayMode, true);
    host.netplay.delay = 6;
    host.netplay.rollback = 12;
    host.netplay.generate_code()?;
    assert!(host.netplay.connection_options()?.is_some());
    assert!(host.rows().iter().any(|row| row.field == F::NetplayRelay));
    assert!(!host.row_applies(F::NetplayBind));
    let mut guest = LauncherState::new(MachineSetup::default());
    guest.toggle_netplay();
    guest.netplay.cycle(F::NetplayMode, true);
    guest.netplay.cycle(F::NetplayPlayer, true);
    guest.begin_edit_netplay(F::NetplayCode);
    for c in host.netplay.code.chars() {
        guest.edit_push(c);
    }
    guest.edit_commit();
    assert_eq!(guest.netplay.delay, 6);
    assert_eq!(guest.netplay.rollback, 12);
    let options = guest.netplay.connection_options()?.unwrap();
    assert_eq!(options.settings().player, 1);
    assert_eq!(options.settings().input_delay, 6);
    assert_eq!(options.settings().rollback_frames, 12);
    assert!(!guest.row_applies(F::NetplayDelay));
    assert!(!guest.row_applies(F::NetplayRelay));
    assert!(!guest.row_applies(F::NetplayNewCode));
    assert!(guest.row_applies(F::NetplayCopyCode));
    assert!(
        !LauncherState::from_raw(&host.setup.to_raw())
            .netplay
            .internet
    );
    host.netplay.delay = 2;
    assert!(host.netplay.connection_options().is_err());
    host.netplay.generate_code()?;
    assert!(host.netplay.connection_options().is_ok());
    host.begin_edit_netplay(F::NetplayRelay);
    host.clear_netplay_edit();
    for c in "https://relay.example.com".chars() {
        host.edit_push(c);
    }
    host.edit_commit();
    assert!(host.netplay.connection_options().is_err());
    host.netplay.generate_code()?;
    assert!(host.netplay.connection_options().is_ok());
    Ok(())
}
