//! cepheus: every Claude Code, Codex, Antigravity, Grok and opencode
//! session in one terminal.
//!
//! A background service owns each session's PTY and terminal emulator; the TUI
//! attaches to it, renders the selected session's real screen and switches
//! between sessions. See `README.md` for the commands.

mod actions;
mod activity;
#[path = "../plugins/agent-update/updater.rs"]
mod agent_update;
mod antigravity;
#[path = "../plugins/api-cost/collector.rs"]
mod api_cost;
mod attention;
mod claude;
mod client;
mod client_temp;
mod clipboard;
mod codex;
mod codexpick;
mod config;
mod ctl;
mod devserver;
mod dot;
mod edit;
mod extensions;
mod external;
mod gitwt;
mod handover;
mod hook;
mod hosts;
mod hotkeys;
mod input;
mod install;
mod installer;
mod job_sessions;
mod keys;
mod kitty;
mod lua;
mod monitor;
mod offload;
mod other;
mod owner_env;
mod pairing;
mod paths;
mod pet;
mod processes;
mod procs;
mod profile;
mod prompt;
mod protocol;
mod providers;
mod recovery_policy;
mod registry;
mod remote;
mod resources;
mod run;
mod scheduler;
mod screen;
mod selection;
mod service;
mod session;
mod skills;
mod startup;
mod status;
mod statusline;
#[path = "../plugins/task-sources/providers.rs"]
mod task_source;
mod theme;
mod titles;
mod tui;
mod util;
mod view;
mod work_queue;
mod wrap;

#[cfg(test)]
#[path = "tests/support.rs"]
mod testsupport;

const USAGE: &str = "\
cepheus — every Claude Code, Codex, Antigravity, Grok and opencode session in one place

usage:
  cepheus                 open the deck (starts the background service if needed)
  cepheus restart         restart the service in place; every session keeps running
  cepheus stop [--yes]    stop an empty service; refuses while sessions are live
  cepheus install         copy this binary to ~/.local/bin/cepheus
  cepheus startup status|enable [--boot|--login]|disable|apply
                          configure future service startup without ending sessions
  cepheus keys            list the hotkeys and how to change them in the config file
  cepheus add <source> [--agents claude,codex,agy] [--dry-run]
                             install a plugin, MCP server or skill for the agents here: a GitHub
                             repo (owner/repo), git URL, folder, npm:<package> or mcp:<name>=<command>
  cepheus uninstall <source> [--agents …]   remove what add installed
  cepheus skills [list|sync|remove <name>]  the deck's skills, linked for Claude, Codex and agy
  cepheus lua [check|example]  load init.lua and the plugins and report errors; an example init.lua
  cepheus dot setup|chat|probe|login  connect to an existing cloud Dot
  cepheus hosts           list the other machines this deck drives and whether they answer
  cepheus hosts install <name> [--ssh <alias>] [--address <ip:port>]... [--label <label>]
                             put this build on a machine over SSH once and drive it from this deck
  cepheus hosts remove <name>
  cepheus run [--host auto|NAME] [--python] [--env K=V]... [--include PATH]... [--artifact PATH]... -- COMMAND
                             run a build or test command in a copy of this worktree on the least busy build host
  cepheus shard [--host auto|NAME,...] --select ci [--base REV] | --runner unittest|django|pytest TARGET...
                             split CI's test selection, or the targets given, across the build hosts
  cepheus ctl status      print the deck state as JSON
  cepheus agent list     list live agents for native communication
  cepheus agent send <id> --stdin  send a peer message without touching a composer
  cepheus ctl fleet <action> [JSON] | admission
  cepheus ctl job start|status|log|stop|dismiss [JSON|-]
  cepheus ctl slot hold|release|status
  cepheus ctl new [--cwd DIR] [--name NAME] [--model M] claude|codex|agy|grok|opencode [host]
  cepheus ctl term [--cwd DIR] [--name NAME] [host] [-- command…]
  cepheus ctl scan [--scope TEXT] [host]   Codex Security scan in a scratch worktree
  cepheus ctl scans | branches | skills
  cepheus ctl install <source> [--agents …] [--host NAME] [--dry-run] [--remove]
  cepheus ctl resume claude|codex|agy|grok|opencode <conversation-id>
  cepheus ctl send <id> <text>      send through the native agent interface
  cepheus ctl keys <id> <keys>      explicit raw terminal keys (escapes: \\r \\n \\t \\e \\\\ \\xHH)
  cepheus ctl broadcast <message>   send a shared instruction to agents currently working
  cepheus ctl screen <id>
  cepheus ctl close|reopen|forget <id>
  cepheus ctl cd <id> <directory>  restart one agent with a fresh chat in another folder
  cepheus ctl conversations
  cepheus --version
  cepheus --build-info    print the compiled release profile as JSON

internal (used by hosted sessions):
  cepheus service         run the service in the foreground
  cepheus remote          run the phone gateway (the service starts it when \"remote\" is on)
  cepheus peer            serve this host's deck to a hub (the service starts it when \"peer_listen\" is set)
  cepheus peer authorize  record the hub key read from standard input
  cepheus peer fingerprint  print this host's peer certificate SHA-256
  cepheus peer setup      the host's half of `hosts install` (hub key on standard input)
  cepheus statusline      Claude Code status line command
  cepheus task-source     a registered Markdown or GitHub task source's snapshot
  cepheus agent-update [--check] [--local] [--tool AGENT] [--config FILE]  update installed agents here and on hosts (:agent-update)
  cepheus hook claude|codex|permission-request|prompt-avoidance
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The service's environment is every hosted program's, and a startup unit
    // records it: never an agent session's (`owner_env.rs`). Before anything
    // reads a path from it or starts a thread.
    if matches!(args.first().map(String::as_str), Some("service" | "startup")) {
        if let Err(err) = owner_env::settle() {
            if args.get(1).map(String::as_str) == Some("--handover") {
                // This process holds every session the previous build passed
                // on; it carries on and reports the error in the deck.
                eprintln!("cepheus service: {err:#}");
                owner_env::record_unsettled(&err);
            } else {
                eprintln!("cepheus {}: {err:#}", args[0]);
                std::process::exit(1);
            }
        }
    }
    let _ = config::Config::load();
    let code = match args.first().map(String::as_str) {
        None | Some("tui") => tui::run(),
        // Asked by the running service before it updates in place: this
        // build must be able to start where it runs and read its state.
        Some("service") if args.get(1).map(String::as_str) == Some("--check-handover") => {
            match service::ServiceOptions::from_env(config::Config::load().0) {
                Ok(_) => args.get(2).map_or(2, |file| handover::check_command(std::path::Path::new(file))),
                Err(err) => {
                    eprintln!("cepheus service: {err:#}");
                    1
                }
            }
        }
        Some("service") => {
            let (cfg, err) = config::Config::load();
            if let Some(err) = err {
                eprintln!("cepheus: config: {err}");
            }
            // `--handover <file>`: the previous build of this process ran
            // this one in its place (`handover.rs`).
            let handover = match (args.get(1).map(String::as_str), args.get(2)) {
                (Some("--handover"), Some(file)) => Some(std::path::PathBuf::from(file)),
                _ => None,
            };
            let started = service::ServiceOptions::from_env(cfg).and_then(|mut opts| {
                opts.handover = handover;
                service::run(opts)
            });
            match started {
                Ok(()) => 0,
                Err(err) => {
                    eprintln!("cepheus service: {err:#}");
                    1
                }
            }
        }
        Some("__job-supervise") => job_sessions::supervisor::command(&args[1..], false),
        Some("__job-exec") => job_sessions::supervisor::command(&args[1..], true),
        Some("remote") => remote::run(),
        Some("peer") => hosts::peer::run(&args[1..]),
        Some("hosts") if args.get(1).map(String::as_str) == Some("coordinator") => client::coordinator::run(&args[2..]),
        Some("hosts") => hosts::install::run(&args[1..]),
        Some("statusline") => statusline::run(),
        Some("hook") => hook::run(&args[1..]),
        Some("ctl") => ctl::run(&args[1..]),
        Some("agent") => ctl::run(&args),
        Some("restart") => ctl::restart(),
        Some("stop") => ctl::stop(args[1..].iter().any(|a| a == "--yes" || a == "-y")),
        Some("install") => install::run(),
        Some("startup") => startup::run(&args[1..]),
        Some("add") => installer::run_cli(&args[1..]),
        Some("uninstall") => {
            let mut rest = args[1..].to_vec();
            rest.push("--remove".into());
            installer::run_cli(&rest)
        }
        Some("skills") => skills::run(&args[1..]),
        Some("run") => offload::run_cli(&args[1..]),
        Some("shard") => offload::shard::cli(&args[1..]),
        Some("lua") => lua::run_cli(&args[1..]),
        Some("dot") => dot::run(&args[1..]),
        Some("task-source") => task_source::run(&args[1..]),
        Some("agent-update") => agent_update::run_cli(&args[1..]),
        Some("keys") => hotkeys::print(),
        Some("help" | "-h" | "--help") => {
            print!("{USAGE}");
            0
        }
        Some("-V" | "--version" | "version") => {
            println!("cepheus {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("--build-info") => {
            println!(
                "{}",
                serde_json::json!({
                    "schema": 1,
                    "program": "cepheus",
                    "version": env!("CARGO_PKG_VERSION"),
                    "profile": env!("CEPHEUS_BUILD_PROFILE"),
                    "opt_level": env!("CEPHEUS_BUILD_OPT_LEVEL"),
                    "debug_assertions": cfg!(debug_assertions),
                })
            );
            0
        }
        Some(other) => {
            eprintln!("cepheus: unknown command {other:?}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}
