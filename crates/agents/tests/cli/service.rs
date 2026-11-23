use super::support::Fixture;
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn prints_project_service_definitions_without_installing() {
    let fixture = Fixture::new();
    let render = |platform| {
        let output = fixture
            .command()
            .args(["service", "--platform", platform])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        String::from_utf8(output.stdout).unwrap()
    };
    let root = fs::canonicalize(&fixture.root).unwrap();
    let systemd = render("systemd");
    assert!(systemd.contains(&format!("WorkingDirectory={}\n", root.display())));
    assert!(systemd.contains("ExecStart=:\""));
    assert!(systemd.contains("WantedBy=default.target"));
    let launchd = render("launchd");
    assert!(launchd.contains(&format!("<string>{}</string>", root.display())));
    assert!(launchd.contains("<key>RunAtLoad</key>"));
    assert!(launchd.contains("<key>KeepAlive</key>"));
    assert_eq!(systemd, render("systemd"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn installs_and_enables_service_through_the_user_manager() {
    let fixture = Fixture::new();
    let home = fixture._dir.path().join("home");
    let config = fixture._dir.path().join("xdg");
    let bin = fixture._dir.path().join("bin");
    let calls = fixture._dir.path().join("manager-calls");
    fs::create_dir(&bin).unwrap();
    let manager = if cfg!(target_os = "linux") {
        "systemctl"
    } else {
        "launchctl"
    };
    let program = bin.join(manager);
    fs::write(
        &program,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$MANAGER_CALLS\"\n[ \"$1\" != print ]\n",
    )
    .unwrap();
    fs::set_permissions(program, fs::Permissions::from_mode(0o700)).unwrap();
    let output = fixture
        .command()
        .args(["service", "install"])
        .env(
            "PATH",
            std::env::join_paths([bin, "/usr/bin".into(), "/bin".into()]).unwrap(),
        )
        .env("XDG_CONFIG_HOME", &config)
        .env("MANAGER_CALLS", &calls)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let printed = String::from_utf8(output.stdout).unwrap();
    let path = std::path::Path::new(printed.trim());
    let calls = fs::read_to_string(calls).unwrap();
    assert!(path.is_file());
    if cfg!(target_os = "linux") {
        assert_eq!(path.parent().unwrap(), config.join("systemd/user"));
        assert!(calls.starts_with("--user daemon-reload\n--user enable --now agents-"));
    } else {
        assert_eq!(path.parent().unwrap(), home.join("Library/LaunchAgents"));
        let calls: Vec<_> = calls.lines().collect();
        assert!(calls[0].starts_with("enable gui/"));
        assert!(calls[1].starts_with("print gui/"));
        assert!(calls[2].starts_with("bootstrap gui/"));
        assert!(calls[2].ends_with(path.to_str().unwrap()));
    }
}
