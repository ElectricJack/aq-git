use std::process::Command;
/// A deliberately small local-only profile. Remote credential profiles are separate capabilities.
pub fn configure(command: &mut Command) {
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/nonexistent")
        .env("XDG_CONFIG_HOME", "/nonexistent")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "0");
    for config in [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "gc.auto=0",
        "pack.threads=2",
        "pack.windowMemory=16m",
        "pack.deltaCacheSize=16m",
        "advice.graftFileDeprecated=false",
        "maintenance.auto=false",
        "core.fsync=objects,reference",
        "core.fsyncMethod=fsync",
        "protocol.ext.allow=never",
        "credential.helper=",
        "core.askPass=",
        "core.pager=cat",
    ] {
        command.arg("-c").arg(config);
    }
}
