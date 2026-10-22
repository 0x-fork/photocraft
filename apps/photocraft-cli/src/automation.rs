use std::io::Write;

use serde_json::Value;

use photocraft_automation::{AuthorizedWorkspace, AutomationError, Headless, PhotocraftMcp, security};

use crate::{
    Result,
    args::{CommandsArgs, ControlArgs, McpArgs, ServeArgs},
    output::{JsonStyle, print_json},
};

pub(crate) fn automation_workspace(args: &ControlArgs) -> Result<AuthorizedWorkspace, AutomationError> {
    AuthorizedWorkspace::new(args.automation_read_root.as_deref(), args.automation_write_root.as_deref())
}

pub(crate) fn commands(a: &CommandsArgs, out: &mut dyn Write) -> Result {
    let h = Headless::new();
    let list = h.command_list();
    let needle = a.filter.as_deref().map(str::to_lowercase);
    let items: Vec<&Value> = list
        .as_array()
        .map(|v| v.iter().collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|c| needle.as_ref().is_none_or(|n| photocraft_engine::commands::list_entry_matches(c, n)))
        .collect();
    if a.json {
        return print_json(out, &Value::Array(items.into_iter().cloned().collect()), JsonStyle::Pretty);
    }
    for c in items {
        let menu = c["menu"].as_array().map(|m| m.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" > ")).unwrap_or_default();
        writeln!(out, "{:<44} {:<32} {}", c["id"].as_str().unwrap_or(""), c["label"].as_str().unwrap_or(""), menu)?;
    }
    Ok(())
}

pub(crate) fn serve(a: &ServeArgs, err: &mut dyn Write) -> Result {
    use std::sync::{Arc, Mutex};
    let h = Arc::new(Mutex::new(Headless::with_workspace(automation_workspace(&a.control)?)));
    match a.port {
        Some(port) => {
            let addr = format!("127.0.0.1:{port}");
            let (supplied, token_file) = security::token_inputs(a.control.control_token.clone(), a.control.control_token_file.clone());
            let token = security::server_token(supplied.as_deref(), token_file.as_deref())?;
            if let Some(path) = &token_file {
                let _ = writeln!(err, "photocraft-cli control token file: {}", path.display());
            } else if supplied.is_none() {
                let _ = writeln!(err, "photocraft-cli control token: {token}");
            }
            photocraft_automation::rpc::serve_tcp(&addr, h, token, |local| {
                let _ = writeln!(err, "photocraft-cli serving on {local}");
            })?;
            Ok(())
        }
        None => {
            let stdin = std::io::stdin();
            Ok(photocraft_automation::rpc::serve_lines(&h, stdin.lock(), std::io::stdout())?)
        }
    }
}

pub(crate) fn mcp(a: &McpArgs) -> Result {
    let server = match &a.bridge {
        Some(addr) => {
            let (supplied, token_file) = security::token_inputs(a.control.control_token.clone(), a.control.control_token_file.clone());
            let token = security::client_token(supplied.as_deref(), token_file.as_deref())?;
            PhotocraftMcp::bridge(addr, &token)?
        }
        None => PhotocraftMcp::headless_with_workspace(automation_workspace(&a.control)?),
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    Ok(rt.block_on(server.serve_stdio())?)
}
