//! The desktop app's login-shell `PATH` finds programs and reaches the programs it starts.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn programs_are_found_and_run_with_the_search_path() {
    let root = std::env::temp_dir().join(format!("blackwall-search-path-{}", std::process::id()));
    let tools = root.join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let probe = tools.join("blackwall-search-probe");
    std::fs::write(&probe, "#!/bin/sh\nprintf '%s' \"$PATH\" > \"$0.path\"\n").unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("mcp.json"),
        r#"{"servers":[{"name":"probe","command":["blackwall-search-probe"]}]}"#,
    )
    .unwrap();

    let before = blackwall_core::mcp::McpServers::start(&root, None, &[]).await;
    assert!(
        before.warnings[0].contains("was not found"),
        "{:?}",
        before.warnings
    );

    blackwall_core::tools::set_search_path(tools.clone().into_os_string());
    let after = blackwall_core::mcp::McpServers::start(&root, None, &[]).await;
    assert!(
        !after.warnings[0].contains("was not found"),
        "{:?}",
        after.warnings
    );
    let seen = std::fs::read_to_string(tools.join("blackwall-search-probe.path")).unwrap();
    assert_eq!(seen, tools.to_string_lossy());
    std::fs::remove_dir_all(root).unwrap();
}
