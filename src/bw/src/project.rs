//! `bw project status|trust|untrust`: review and trust the current project's settings file.
use blackwall_core::project::{self, ProjectStatus, RuleView, Trust};
use std::{
    io::{BufRead, IsTerminal, Write},
    path::Path,
};

const USAGE: &str = "Use bw project [status] | trust [--yes] | untrust.";

pub fn run(directory: &Path, workspace: &Path, arguments: &[String]) -> Result<(), String> {
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    match arguments.as_slice() {
        [] | ["status"] => {
            print!("{}", describe(&project::status(workspace, directory)));
            Ok(())
        }
        ["trust"] => trust(directory, workspace, false),
        ["trust", "--yes"] => trust(directory, workspace, true),
        ["untrust"] => {
            let status = project::status(workspace, directory);
            if project::untrust(directory, &status.project).map_err(|e| e.to_string())? {
                println!("This project's settings are no longer trusted.");
            } else {
                println!("This project was not trusted.");
            }
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn trust(directory: &Path, workspace: &Path, yes: bool) -> Result<(), String> {
    let status = project::status(workspace, directory);
    if let Some(error) = &status.error {
        return Err(safe(error));
    }
    let (Some(content), Some(sha256)) = (&status.content, &status.sha256) else {
        return Err(format!(
            "There is no project settings file at {}.",
            safe(&status.path)
        ));
    };
    if status.trust == Trust::Trusted {
        println!("This project's settings are already trusted.");
        return Ok(());
    }
    if !yes && !std::io::stdin().is_terminal() {
        return Err(
            "Trusting project settings needs a terminal to confirm. Review the file, then run bw project trust --yes."
                .into(),
        );
    }
    println!("{}\n", safe(&status.path));
    print!("{}", rules(&status));
    if let Trust::Changed { diff } = &status.trust {
        println!("\nChanges since you trusted it:\n{}", safe(diff));
    }
    if !matches!(&status.trust, Trust::Changed { diff } if diff != project::DIFF_TOO_LARGE) {
        println!("\nFile:\n{}", safe(content));
    }
    if !yes {
        print!(
            "Trust these project settings? Its allow rules and other settings will apply. [y/N] "
        );
        std::io::stdout()
            .flush()
            .map_err(|_| "Could not write terminal output.")?;
        let mut answer = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|_| "Could not read terminal input.")?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
            println!("Not trusted.");
            return Ok(());
        }
    }
    // Trust only the bytes shown above; a file changed since is refused.
    project::trust(workspace, directory, sha256).map_err(|error| error.to_string())?;
    println!("Trusted. Editing the file makes it untrusted again.");
    Ok(())
}

fn describe(status: &ProjectStatus) -> String {
    let mut text = format!(
        "Project: {}\n",
        safe(&project::display_path(&status.project))
    );
    if !status.exists {
        text.push_str(&format!(
            "No project settings file ({}).\n",
            safe(&status.path)
        ));
        return text;
    }
    text.push_str(&format!("Settings file: {}\n", safe(&status.path)));
    let trust = match &status.trust {
        Trust::None => "not trusted",
        Trust::Trusted => "trusted",
        Trust::Changed { .. } => "changed since you trusted it (untrusted until trusted again)",
    };
    text.push_str(&format!("Trust: {trust}\n"));
    if let Some(error) = &status.error {
        text.push_str(&format!("Problem: {}\n", safe(error)));
    }
    text.push_str(&rules(status));
    let list = |lines: &[String]| {
        if lines.is_empty() {
            "nothing".to_owned()
        } else {
            safe(&lines.join("; "))
        }
    };
    text.push_str(&format!("Applies now: {}\n", list(&status.applies)));
    if !status.waiting.is_empty() {
        text.push_str(&format!(
            "Waiting for trust: {}\nRun bw project trust to review and trust it.\n",
            list(&status.waiting)
        ));
    }
    if let Trust::Changed { diff } = &status.trust {
        text.push_str(&format!("Changes since you trusted it:\n{}", safe(diff)));
    }
    text
}

/// Repository text for the terminal: control characters dropped, so no escape sequences reach
/// it, and invisible format characters such as bidi controls shown as `\u{XXXX}`.
pub(crate) fn safe(text: &str) -> String {
    crate::memory::visible(&project::escape_invisible(text))
}

/// The file's rules as a table, in file order, with badges for broad rules.
fn rules(status: &ProjectStatus) -> String {
    if status.rules.is_empty() {
        return String::new();
    }
    let name = |value: serde_json::Result<serde_json::Value>| {
        value
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    let mode = name(serde_json::to_value(status.mode));
    let mut text = format!("Rules ({mode}):\n  #  decision  kind     subject\n");
    for (index, rule) in status.rules.iter().enumerate() {
        let RuleView {
            decision,
            kind,
            subject,
            badges,
        } = rule;
        let badges = if badges.is_empty() {
            String::new()
        } else {
            format!("  [{}]", badges.join(", "))
        };
        text.push_str(&format!(
            "  {index:<2} {:<9} {:<8} {}{badges}\n",
            name(serde_json::to_value(decision)),
            name(serde_json::to_value(kind)),
            safe(subject)
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(trust: Trust) -> ProjectStatus {
        ProjectStatus {
            project: "/work".into(),
            path: "/work/.blackwall/project.json".into(),
            exists: true,
            trust,
            sha256: Some("abc".into()),
            content: Some("{}".into()),
            rules: vec![RuleView {
                decision: blackwall_core::policy::Decision::Allow,
                kind: blackwall_core::ApprovalKind::File,
                subject: ".".into(),
                badges: vec!["matches every file".into()],
            }],
            mode: blackwall_core::policy::RuleMode::Ordered,
            applies: vec!["1 deny and 0 ask rules".into()],
            waiting: vec!["1 allow rule".into()],
            error: None,
        }
    }

    #[test]
    fn status_shows_trust_and_what_applies_now() {
        let text = describe(&status(Trust::None));
        assert!(text.contains("Trust: not trusted"), "{text}");
        assert!(
            text.contains("Applies now: 1 deny and 0 ask rules"),
            "{text}"
        );
        assert!(text.contains("Waiting for trust: 1 allow rule"), "{text}");
        let text = describe(&status(Trust::Changed {
            diff: "+  \"x\"\n".into(),
        }));
        assert!(text.contains("changed since you trusted it"), "{text}");
        assert!(text.ends_with("+  \"x\"\n"), "{text}");
        let text = describe(&status(Trust::None));
        assert!(text.contains("Rules (ordered):"), "{text}");
        assert!(
            text.contains("  0  allow     file     .  [matches every file]"),
            "{text}"
        );
        let mut missing = status(Trust::None);
        missing.exists = false;
        assert!(describe(&missing).contains("No project settings file"));
    }

    #[test]
    fn terminal_escapes_and_bidi_are_neutralised() {
        let mut shown = status(Trust::Changed {
            diff: "+ \u{1b}[2J\u{202E}evil\n".into(),
        });
        shown.project = r"\\?\C:\work".into();
        shown.rules[0].subject = "rm\u{200B}x".into();
        let text = describe(&shown);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{202E}'),
            "{text}"
        );
        assert!(text.contains("\\u{202E}evil"), "{text}");
        assert!(text.contains("rm\\u{200B}x"), "{text}");
        assert!(text.contains(r"Project: C:\work"), "{text}");
    }
}
