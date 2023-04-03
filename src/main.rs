#[macro_use] extern crate serde;
use std::{env::{self, current_dir}, cell::RefCell};

use anyhow::anyhow;
use clap::{Subcommand, Parser, Args};
use async_channel::{Receiver, Sender};
use log::info;
use smarther::{model::{PlantDetail, ModuleStatus}, AuthorizationInfo, SmartherApi, states::{Unauthorized}};
use tokio_util::sync::CancellationToken;

use crate::{token_watchdog::token_refresher, mqtt::mqtt_handler, webhook::webhook_handler};

mod token_watchdog;
mod mqtt;
mod webhook;

#[derive(Parser)]
struct SmartherBridgeArgs {
    #[clap(subcommand)]
    command: Commands
} 

#[derive(Subcommand)]
enum Commands {
    Setup {
        #[clap(flatten)]
        setup_args: SetupArgs
    },
    Run
}

#[derive(Args)]
struct SetupArgs {
    #[clap( long)]
    client_id: Option<String>,
    #[clap(long)]
    client_secret: Option<String>,
    #[clap(long)]
    subkey: Option<String>,
    #[clap(long)]
    base_uri: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
struct CachedTopology {
    plants: Vec<PlantDetail>
}

struct Context {
    configuration: BridgeConfiguration,
    topology_cache: CachedTopology,
    auth_info: RefCell<AuthorizationInfo>,
    reset_refresh_watchdog: (Sender<()>, Receiver<()>),
    status_updates: (Sender<ModuleStatus>, Receiver<ModuleStatus>),
    auth_file: String,
}

impl Context {
    pub async fn refresh_token_if_needed(&self) -> anyhow::Result<()> {
        let auth_info = self.auth_info.borrow().clone();
        let client = SmartherApi::default();
        let refreshed = refresh_token_if_needed(&client, auth_info, &self.auth_file).await?;
        self.auth_info.replace(refreshed);
        self.reset_refresh_watchdog.0.send(()).await?;
        Ok(())
    }

    async fn wait_token_reset(&self) -> anyhow::Result<()> {
        self.reset_refresh_watchdog.1.recv().await?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
struct BridgeConfiguration {
    #[serde(skip_serializing_if = "Option::is_none")]
    webhook_endpoint: Option<String>,
    #[serde(default = "BridgeConfiguration::default_base_topic")]
    mqtt_base_topic: String,
    #[serde(default = "BridgeConfiguration::default_mqtt_broker")]
    mqtt_broker: String,
    #[serde(default = "BridgeConfiguration::default_mqtt_port")]
    mqtt_port: u16,
    #[serde(default = "BridgeConfiguration::default_mqtt_username")]
    mqtt_username: String,
    #[serde(default = "BridgeConfiguration::default_mqtt_password")]
    mqtt_password: String,
    #[serde(default = "BridgeConfiguration::default_listen_port")]
    listen_port: u16,
    #[serde(default = "BridgeConfiguration::default_listen_host")]
    listen_host: String,
}

impl Default for BridgeConfiguration {
    fn default() -> Self {
        Self { 
            webhook_endpoint: None, 
            mqtt_base_topic: BridgeConfiguration::default_base_topic(), 
            mqtt_broker: BridgeConfiguration::default_mqtt_broker(), 
            mqtt_port: BridgeConfiguration::default_mqtt_port(), 
            mqtt_username: BridgeConfiguration::default_mqtt_username(), 
            mqtt_password: BridgeConfiguration::default_mqtt_password(),
            listen_port: BridgeConfiguration::default_listen_port(),
            listen_host: BridgeConfiguration::default_listen_host()
        }
    }
}

impl BridgeConfiguration {
    fn default_base_topic() -> String {
        "smarther".to_string()
    }
    
    fn default_mqtt_broker() -> String {
        "localhost".to_string()
    }
    
    fn default_mqtt_port() -> u16 {
        1883
    }
    
    fn default_mqtt_username() -> String {
        "anonymous".to_string()
    }
    
    fn default_mqtt_password() -> String {
        "".to_string()
    }

    fn default_listen_port() -> u16 {
        8080
    }

    fn default_listen_host() -> String {
        "localhost".to_string()
    }
}

fn load_auth_info(auth_file: &str) -> anyhow::Result<AuthorizationInfo> {
    let auth_info_json = std::fs::read_to_string(auth_file)?;
    let auth_info: AuthorizationInfo = serde_json::from_str(&auth_info_json)?;
    Ok(auth_info)
}

async fn refresh_token_if_needed(client: &SmartherApi<Unauthorized>, auth_info: AuthorizationInfo, auth_file: &str) -> anyhow::Result<AuthorizationInfo> {
    if auth_info.is_refresh_needed() {
        let refreshed_auth_info = client.refresh_token(&auth_info).await?;
        let refreshed_auth_info_json = serde_json::to_string_pretty(&auth_info)?;
        std::fs::write(auth_file, refreshed_auth_info_json)?;
        return Ok(refreshed_auth_info);
    }

    Ok(auth_info)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = SmartherBridgeArgs::parse();
    let config_dir = env::var("SMARTHER_CONFIG_DIR").unwrap_or_else(|_| current_dir().unwrap().to_string_lossy().into());
    let auth_file = format!("{}/tokens.json", config_dir);
    let plant_topology_file = format!("{}/plant_topology.json", config_dir);
    let subscriptions_file = format!("{}/subscriptions.json", config_dir);
    let configuration_file = format!("{}/configuration.json", config_dir);

    match &args.command {
        Commands::Setup { setup_args } => {
            setup(setup_args, &auth_file, &plant_topology_file, &configuration_file).await?;
            
        },
        _ => {
            run(auth_file, plant_topology_file, subscriptions_file, configuration_file).await?;
        }
    }

    Ok(())
}

async fn setup(setup_args: &SetupArgs, auth_file: &str, topology_file: &str, configuration_file: &str) -> anyhow::Result<()> {
    let client = SmartherApi::default();

    let configuration = if let Ok(configuration_content) = std::fs::read_to_string(configuration_file) {
        serde_json::from_str(&configuration_content)?
    } else {
        BridgeConfiguration::default()
    };

    let auth_info = if let Ok(auth_info) = load_auth_info(auth_file) {
        auth_info
    } else { 
        match setup_args {
            SetupArgs{ client_id: Some(client_id), client_secret: Some(client_secret), subkey: Some(subkey), base_uri } => {
                let auth_info = client.get_oauth_access_code(client_id, client_secret, base_uri.as_deref(), subkey, (&configuration.listen_host, configuration.listen_port)).await?;
                let auth_info_json = serde_json::to_string_pretty(&auth_info)?;
                std::fs::write(auth_file, auth_info_json)?;
                auth_info
            },
            _ => {
                return Err(anyhow!("Please provide client_id, client_secret and subkey"));
            }
        }
    };

    let auth_info = refresh_token_if_needed(&client, auth_info, auth_file).await?;
    let client = client.with_authorization(auth_info)?;

    let mut topology = vec!();
    let plants = client.get_plants().await?;
    for plant in &plants.plants {
        let plant_detail = client.get_topology(&plant.id).await?;
        topology.push(plant_detail.plant);
    }
    let topology_json = serde_json::to_string_pretty(&CachedTopology { plants: topology })?;
    std::fs::write(topology_file, topology_json)?;
    info!("Setup completed");

    Ok(())
}

async fn run(auth_file: String, topology_file: String, subscriptions_file: String, configuration_file: String) -> anyhow::Result<()> {
    let auth_info = RefCell::new(load_auth_info(&auth_file)?);
    let topology_cache = std::fs::read_to_string(&topology_file)?;
    let topology_cache: CachedTopology = serde_json::from_str(&topology_cache)?;

    let configuration = if let Ok(configuration_content) = std::fs::read_to_string(&configuration_file) {
        serde_json::from_str(&configuration_content)?
    } else {
        BridgeConfiguration::default()
    };

    //Save configuration
    let configuration_json = serde_json::to_string_pretty(&configuration)?;
    std::fs::write(configuration_file, configuration_json)?;

    //Create context and run
    let context = Context {
        configuration,
        topology_cache,
        auth_info,
        reset_refresh_watchdog: async_channel::bounded(1),
        status_updates: async_channel::unbounded(),
        auth_file
    };

    let cancellation_token = CancellationToken::new();
    tokio::join!(
        interrupt_handler(cancellation_token.clone()),
        webhook_handler(&context, cancellation_token.clone()),
        mqtt_handler(&context, cancellation_token.clone()),
        token_refresher(&context, cancellation_token.clone())
    );

    Ok(())
}

async fn interrupt_handler(cancellation_token: CancellationToken) {
    tokio::signal::ctrl_c().await.unwrap();
    cancellation_token.cancel();
}