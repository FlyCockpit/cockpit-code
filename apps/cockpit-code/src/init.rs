use clap::Args;
use code_config::Directories;
use serde_json::{Value, json};
use std::{
    error::Error,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Args)]
pub struct InitArgs {
    /// Write a provider template (repeatable): openrouter, openrouter-anthropic, openai, openai-chat, anthropic, command.
    #[arg(long, value_name = "TEMPLATE")]
    provider: Vec<String>,
    /// Environment variable NAME; only a ${NAME} reference is written.
    #[arg(long, value_name = "NAME")]
    key_env: Option<String>,
    #[arg(long, value_name = "PROVIDER/MODEL", help = "Default model as provider/model")]
    default_model: Option<String>,
    /// Default permission mode written to config.json.
    #[arg(long, value_parser=["manual","auto","unattended","yolo"])]
    default_mode: Option<String>,
    /// Safety model that reviews actions in auto and unattended modes.
    #[arg(long, value_name = "PROVIDER/MODEL")]
    safety_model: Option<String>,
    /// Overwrite existing files instead of skipping them.
    #[arg(long)]
    force: bool,
    /// Also install the config skill into claude, codex or a skills directory path.
    #[arg(long, value_name = "TARGET")]
    skill_target: Vec<String>,
    /// Print the config skill text after setup.
    #[arg(long)]
    print_skill: bool,
}
fn write(
    path: &Path,
    value: &Value,
    force: bool,
    created: &mut Vec<PathBuf>,
    skipped: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(&serde_json::to_vec_pretty(value)?)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            created.push(path.into());
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => skipped.push(path.into()),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn provider(template: &str, key: Option<&str>) -> Result<Value, Box<dyn Error>> {
    let (kind, url, env, auth) = match template {
        "openrouter" | "openrouter-chat" => (
            "openai-chat",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            "Authorization",
        ),
        "openrouter-anthropic" => (
            "anthropic",
            "https://openrouter.ai/api",
            "OPENROUTER_API_KEY",
            "x-api-key",
        ),
        "openai" | "openai-responses" => (
            "openai-responses",
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            "Authorization",
        ),
        "openai-chat" => (
            "openai-chat",
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            "Authorization",
        ),
        "anthropic" => (
            "anthropic",
            "https://api.anthropic.com",
            "ANTHROPIC_API_KEY",
            "x-api-key",
        ),
        "command" => {
            return Ok(
                json!({"type":"command","command":{"argv":["YOUR_PROVIDER_EXECUTABLE"],"stdin":"{requestJson}","history":"full","output":"jsonl"},"models":{}}),
            );
        }
        _ => return Err(format!("unknown provider template {template}").into()),
    };
    let key = key.unwrap_or(env);
    if key.is_empty()
        || !key
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
    {
        return Err("--key-env must be an environment variable name".into());
    }
    let reference = format!("${{{key}}}");
    let header = if auth == "Authorization" {
        format!("Bearer {reference}")
    } else {
        reference
    };
    let mut headers = serde_json::Map::new();
    headers.insert(auth.into(), json!(header));
    if kind == "anthropic" {
        headers.insert("anthropic-version".into(), json!("2023-06-01"));
    }
    Ok(json!({"type":kind,"baseUrl":url,"headers":headers,"cacheTtlSecs":300,"models":{}}))
}
pub fn run(args: &InitArgs, json_output: bool, cwd: &Path) -> Result<(), Box<dyn Error>> {
    // Validate everything before creating any files.
    let mut providers = args
        .provider
        .iter()
        .map(|name| Ok((name.clone(), provider(name, args.key_env.as_deref())?)))
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    for id in [&args.default_model, &args.safety_model]
        .into_iter()
        .flatten()
    {
        let (p, m) = id
            .split_once('/')
            .filter(|(p, m)| !p.is_empty() && !m.is_empty())
            .ok_or("model must be provider/model")?;
        for (name, value) in &mut providers {
            if name == p {
                value["models"][m] = json!({"description":"Configure verified model metadata; see the config skill models.dev reference.","maxOutputTokens":8192,"capabilities":{"input":["text"],"output":["text"],"tools":true}});
                if value["type"] == "anthropic" {
                    value["models"][m]["thinking"] = json!("none");
                }
            }
        }
    }
    let dirs = Directories::resolve()?;
    fs::create_dir_all(dirs.config.join("providers"))?;
    fs::create_dir_all(dirs.config.join("mcps"))?;
    let mut created = vec![];
    let mut skipped = vec![];
    let mut config = serde_json::to_value(code_config::Config::default())?;
    if let Some(id) = &args.default_model {
        config["defaultModel"] = json!(id);
    }
    if let Some(mode) = &args.default_mode {
        config["permissions"]["mode"] = json!(mode);
    }
    if let Some(id) = &args.safety_model {
        config["permissions"]["autoModel"] = json!(id);
    }
    write(
        &dirs.config.join("config.json"),
        &config,
        args.force,
        &mut created,
        &mut skipped,
    )?;
    for (name, value) in providers {
        write(
            &dirs.config.join("providers").join(format!("{name}.json")),
            &value,
            args.force,
            &mut created,
            &mut skipped,
        )?;
    }
    fs::create_dir_all(dirs.config.join("guidance"))?;
    for (name, text) in code_engine::guidance_library::starter_files() {
        let path = dirs.config.join("guidance").join(name);
        if path.exists() && !args.force {
            skipped.push(path);
        } else {
            let mut options = fs::OpenOptions::new();
            options.write(true);
            if args.force {
                options.create(true).truncate(true);
            } else {
                options.create_new(true);
            }
            let mut file = match options.open(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    skipped.push(path);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            created.push(path);
        }
    }
    let tools_path = dirs.config.join("tools.json");
    if code_engine::tool_defaults::install(&dirs.config, &dirs.data, args.force)? {
        created.push(tools_path);
    } else {
        skipped.push(tools_path);
    }
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    created.extend(code_engine::bundled_skills::install(&home, &dirs.data)?);
    for target in &args.skill_target {
        let target = match target.as_str() {
            "claude" => home.join(".claude/skills"),
            "codex" => home.join(".codex/skills"),
            path => path
                .strip_prefix("~/")
                .map(|p| home.join(p))
                .unwrap_or_else(|| PathBuf::from(path)),
        };
        created.extend(code_engine::bundled_skills::install_at(
            &target, &dirs.data,
        )?);
    }
    let store = code_config::ConfigStore::load(dirs.clone())?;
    let detection = code_engine::lsp_defaults::resolve(&store.snapshot().config.lsp, cwd);
    if json_output {
        println!(
            "{}",
            json!({"created":created,"skipped":skipped,"lsp":detection,"next":"cockpit-code doctor","skill":args.print_skill.then(code_engine::bundled_skills::config_skill)})
        );
    } else {
        for path in created {
            println!("created {}", path.display());
        }
        for path in skipped {
            println!("skipped {}", path.display());
        }
        if args.print_skill {
            println!("{}", code_engine::bundled_skills::config_skill());
        }
        for (id, server) in detection.servers {
            println!("lsp {id}: detected {}", server.command);
        }
        for hint in detection.hints {
            println!("lsp install hint: {hint}");
        }
        if detection.scan_truncated {
            println!("lsp: language scan capped at 100000 entries");
        }
        println!("Next: cockpit-code doctor");
    }
    Ok(())
}
