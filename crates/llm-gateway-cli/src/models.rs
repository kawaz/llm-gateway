//! upstream に聞いて、実際に公開されるモデルを出す。

use std::path::Path;
use std::process::ExitCode;

use llm_gateway::Gateway;
use llm_gateway::credential::file::FileStore;

use crate::failure::Failure;
use crate::load;

pub fn run(config_path: &Path) -> Result<ExitCode, Failure> {
    let config = load(config_path)?;

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| Failure::from(format!("could not start the async runtime: {e}")))?;

    runtime.block_on(async move {
        let store = FileStore::open(config.store.resolve_dir())
            .map_err(|e| Failure::from(e.to_string()))?;
        let gateway = Gateway::new(&config, store).map_err(|e| Failure::from(e.to_string()))?;
        gateway.refresh_models().await;

        let names: Vec<String> = gateway
            .namespace_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut any = false;

        for (i, ns_name) in names.iter().enumerate() {
            let Some(ns) = gateway.namespace(ns_name) else {
                continue;
            };
            let models = gateway.models(ns).await;
            if models.is_empty() {
                continue;
            }
            any = true;

            // namespace が 1 つだけなら見出しは邪魔。
            if names.len() > 1 {
                if i > 0 {
                    println!();
                }
                println!("[{ns_name}]");
            }
            for model in &models {
                let route = gateway.route_names(ns, model).await.join(" → ");
                println!("{model}\t{route}");
            }
        }

        if !any {
            println!("no models can be served.");
            println!(
                "check that credentials are in place, and that exclude is not hiding all of them."
            );
            return Ok(ExitCode::FAILURE);
        }
        Ok(ExitCode::SUCCESS)
    })
}
