//! CLI integration tests

#[cfg(test)]
mod tests {
    use std::process::Command;

    fn chatstronomy() -> Command {
        // Cargo already builds the package binary for integration tests and
        // exposes its exact path here. Spawning nested `cargo run` processes
        // made these tiny help checks contend for Cargo's target lock and take
        // more than a minute on Windows release runners.
        Command::new(env!("CARGO_BIN_EXE_chatstronomy"))
    }

    fn help_text() -> String {
        let output = chatstronomy()
            .arg("--help")
            .output()
            .expect("Failed to execute command");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn test_help_command() {
        let stdout = help_text();
        assert!(stdout.contains("Chatstronomy"));
        // Always assert something structural. Every other assertion here is
        // gated on a platform or a feature, and on Linux with
        // --no-default-features they all compile out — which once left this
        // suite passing against a binary that exposed no subcommands at all.
        assert!(stdout.contains("Usage:"));
    }

    #[test]
    fn test_basic_commands_available() {
        let stdout = help_text();
        assert!(stdout.contains("licenses"));

        // `plugin-runtime` speaks over a Windows named pipe, so the subcommand
        // only exists there. Assert it is absent elsewhere rather than simply
        // skipping the check.
        #[cfg(windows)]
        assert!(stdout.contains("plugin-runtime"));
        #[cfg(not(windows))]
        assert!(!stdout.contains("plugin-runtime"));

        #[cfg(feature = "hub")]
        assert!(stdout.contains("hub"));
        #[cfg(not(feature = "hub"))]
        assert!(!stdout.contains("\n  hub"));
    }

    #[test]
    fn redistributed_executables_include_application_and_font_notices() {
        let output = chatstronomy()
            .arg("licenses")
            .output()
            .expect("Failed to read embedded license notices");
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("Apache License"));
        assert!(text.contains("SIL OPEN FONT LICENSE Version 1.1"));
        assert!(text.contains("Copyright (c) 2012 Red Hat, Inc."));
    }
}
