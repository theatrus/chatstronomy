use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "chatstronomy")]
#[command(about = "Chatstronomy — pipe your observatory into chat")]
#[command(version = chatstronomy::version::VERSION_STRING)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Print the application and embedded font copyright and license notices.
    Licenses,
    /// Print the machine-readable runtime and protocol compatibility contract.
    #[command(name = "artifact-contract", hide = true)]
    ArtifactContract,
    /// Run the centralized Hub service.
    #[cfg(feature = "hub")]
    Hub {
        #[arg(long = "hub-config", default_value = "hub.json")]
        hub_config: String,
        /// Write a default Hub configuration and exit.
        #[arg(long)]
        init: bool,
    },
    /// Run a plugin-owned local Direct process configured over a secure pipe.
    #[cfg(windows)]
    PluginRuntime {
        #[arg(long)]
        bootstrap_pipe: String,
        #[arg(long)]
        log_file: String,
    },
    /// Internal Direct transport and chart-rendering diagnostic.
    #[cfg(windows)]
    #[command(name = "direct-render-probe", hide = true)]
    DirectRenderProbe {
        #[arg(long)]
        pipe_name: String,
        #[arg(long)]
        guider_output: String,
        #[arg(long)]
        autofocus_output: Option<String>,
    },
    /// Internal end-to-end Hub diagnostic used by the plugin tests.
    #[cfg(all(windows, feature = "hub"))]
    #[command(name = "direct-hub-probe", hide = true)]
    DirectHubProbe {
        #[arg(long)]
        guider_output: String,
        #[arg(long)]
        autofocus_output: String,
    },
}

#[tokio::main]
async fn main() {
    let result = match Cli::parse().command {
        Commands::Licenses => {
            println!(
                "Chatstronomy — Apache License 2.0\n\n{}\n\nLiberation Sans — SIL Open Font License 1.1\n\n{}",
                include_str!("../LICENSE"),
                include_str!("../assets/LiberationSans-LICENSE")
            );
            Ok(())
        }
        Commands::ArtifactContract => chatstronomy::artifact_contract::json()
            .map(|json| println!("{json}"))
            .map_err(|error| error.into()),
        #[cfg(feature = "hub")]
        Commands::Hub { hub_config, init } => cmd_hub(&hub_config, init).await,
        #[cfg(windows)]
        Commands::PluginRuntime {
            bootstrap_pipe,
            log_file,
        } => chatstronomy::plugin_runtime::run_from_named_pipe(&bootstrap_pipe, &log_file).await,
        #[cfg(windows)]
        Commands::DirectRenderProbe {
            pipe_name,
            guider_output,
            autofocus_output,
        } => cmd_direct_render_probe(&pipe_name, &guider_output, autofocus_output.as_deref()).await,
        #[cfg(all(windows, feature = "hub"))]
        Commands::DirectHubProbe {
            guider_output,
            autofocus_output,
        } => cmd_direct_hub_probe(&guider_output, &autofocus_output).await,
    };

    if let Err(error) = result {
        eprintln!("Chatstronomy failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(feature = "hub")]
async fn cmd_hub(config_path: &str, init: bool) -> Result<(), Box<dyn std::error::Error>> {
    use chatstronomy::hub::{config::HubConfig, server};

    if init {
        if std::path::Path::new(config_path).exists() {
            return Err(format!("Refusing to overwrite existing file {config_path}").into());
        }
        HubConfig::default().save_to_file(config_path)?;
        println!("Wrote default Hub configuration to {config_path}");
        return Ok(());
    }

    let config = HubConfig::load_from_file(config_path).map_err(|error| {
        format!("{error}. Run `chatstronomy hub --init` to create a configuration.")
    })?;
    config.validate()?;
    server::run(config).await?;
    Ok(())
}

#[cfg(windows)]
async fn cmd_direct_render_probe(
    pipe_name: &str,
    guider_output: &str,
    autofocus_output: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    use chatstronomy::direct::pipe_source::DirectPipeRigSource;
    use chatstronomy::source::{RigCapabilities, RigSource};

    let source = DirectPipeRigSource::connect(pipe_name, RigCapabilities::all()).await?;
    let graph = source.get_guider_graph().await?;
    let png = chatstronomy::charts::render_guider_graph_png(&graph.response)?;
    std::fs::write(guider_output, png)?;
    if let Some(output) = autofocus_output {
        let autofocus = source.get_last_autofocus().await?;
        let png = chatstronomy::charts::render_autofocus_graph_png(&autofocus.response)?;
        std::fs::write(output, png)?;
    }
    Ok(())
}

#[cfg(all(windows, feature = "hub"))]
async fn cmd_direct_hub_probe(
    guider_output: &str,
    autofocus_output: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use chatstronomy::hub::{
        config::HubConfig,
        db::Db,
        direct_source::DirectRigSource,
        server::{self, HubState},
        store::UserRow,
    };
    use chatstronomy::source::RigSource;
    use std::io::Write;
    use std::time::Duration;

    let db = Db::open_in_memory()?;
    db.upsert_user(&UserRow {
        discord_user_id: 1,
        username: "N.I.N.A. probe".to_string(),
        email: None,
        email_verified: false,
        avatar_url: None,
    })?;
    db.register_guild(100, "Probe observatory", 1)?;
    let telescope = db.create_telescope(1, "N.I.N.A. hosted probe")?;
    let pairing_token = db.issue_pairing_token(telescope.id, 1)?;
    let state = HubState::build(HubConfig::default(), db, None)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = server::router(state.clone());
    let server_task = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });

    println!(
        "{}",
        serde_json::json!({
            "probe": "direct_hub_ready",
            "hub_url": format!("http://{address}"),
            "pairing_token": pairing_token,
        })
    );
    std::io::stdout().flush()?;

    let connection = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(connection) = state.rig_connections.get(telescope.id) {
                break connection;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| "N.I.N.A. plugin did not connect to the Hub probe")?;
    let source = DirectRigSource::new(connection);
    let guider = source.get_guider_graph().await?;
    std::fs::write(
        guider_output,
        chatstronomy::charts::render_guider_graph_png(&guider.response)?,
    )?;
    let autofocus = source.get_last_autofocus().await?;
    std::fs::write(
        autofocus_output,
        chatstronomy::charts::render_autofocus_graph_png(&autofocus.response)?,
    )?;

    println!("{}", serde_json::json!({"probe": "direct_hub_complete"}));
    std::io::stdout().flush()?;
    server_task.abort();
    Ok(())
}
