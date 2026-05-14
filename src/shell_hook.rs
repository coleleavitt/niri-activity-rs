use crate::error::Error;

/// Generate shell hook code for the given shell type.
pub fn print_hook(shell: &str) -> Result<(), Error> {
    let output = generate_hook(shell)?;
    print!("{}", output);
    Ok(())
}

fn generate_hook(shell: &str) -> Result<String, Error> {
    match shell {
        "bash" => Ok(bash_hook()),
        "zsh" => Ok(zsh_hook()),
        "fish" => Ok(fish_hook()),
        other => Err(Error::InvalidArgument(format!(
            "unsupported shell '{}': expected bash, zsh, or fish",
            other
        ))),
    }
}

fn bash_hook() -> String {
    r#"# niri-activity-rs shell integration
__niri_activity_prompt_command() {
    printf '%s\n' "$PWD" > "${XDG_DATA_HOME:-$HOME/.local/share}/niri-activity-rs/current_pwd"
}
if [[ "$PROMPT_COMMAND" != *"__niri_activity_prompt_command"* ]]; then
    PROMPT_COMMAND="__niri_activity_prompt_command;${PROMPT_COMMAND:+ $PROMPT_COMMAND}"
fi
"#
    .to_string()
}

fn zsh_hook() -> String {
    r#"# niri-activity-rs shell integration
__niri_activity_precmd() {
    printf '%s\n' "$PWD" > "${XDG_DATA_HOME:-$HOME/.local/share}/niri-activity-rs/current_pwd"
}
if [[ "$precmd_functions" != *"__niri_activity_precmd"* ]]; then
    precmd_functions+=(__niri_activity_precmd)
fi
"#
    .to_string()
}

fn fish_hook() -> String {
    r#"function __niri_activity_pwd --on-event fish_prompt
    printf '%s\n' $PWD > (set -q XDG_DATA_HOME; and echo $XDG_DATA_HOME; or echo $HOME/.local/share)/niri-activity-rs/current_pwd
end
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_hook_contains_prompt_command() {
        let hook = bash_hook();
        assert!(
            hook.contains("PROMPT_COMMAND"),
            "bash hook must reference PROMPT_COMMAND"
        );
    }

    #[test]
    fn bash_hook_contains_current_pwd() {
        let hook = bash_hook();
        assert!(
            hook.contains("current_pwd"),
            "bash hook must write to current_pwd"
        );
    }

    #[test]
    fn zsh_hook_contains_precmd() {
        let hook = zsh_hook();
        assert!(
            hook.contains("precmd_functions"),
            "zsh hook must reference precmd_functions"
        );
    }

    #[test]
    fn zsh_hook_contains_current_pwd() {
        let hook = zsh_hook();
        assert!(
            hook.contains("current_pwd"),
            "zsh hook must write to current_pwd"
        );
    }

    #[test]
    fn fish_hook_contains_fish_prompt() {
        let hook = fish_hook();
        assert!(
            hook.contains("fish_prompt"),
            "fish hook must reference fish_prompt event"
        );
    }

    #[test]
    fn fish_hook_contains_current_pwd() {
        let hook = fish_hook();
        assert!(
            hook.contains("current_pwd"),
            "fish hook must write to current_pwd"
        );
    }

    #[test]
    fn unsupported_shell_returns_error() {
        let result = generate_hook("powershell");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("powershell"));
    }

    #[test]
    fn generate_bash_succeeds() {
        let result = generate_hook("bash");
        assert!(result.is_ok());
    }

    #[test]
    fn generate_zsh_succeeds() {
        let result = generate_hook("zsh");
        assert!(result.is_ok());
    }

    #[test]
    fn generate_fish_succeeds() {
        let result = generate_hook("fish");
        assert!(result.is_ok());
    }
}
