use actix_web::{post, web::{Data, self}, HttpServer, App, error, HttpResponse};
use async_channel::Sender;
use log::{error, warn, info, debug};
use smarther::{model::ModuleStatus, SmartherApi};
use tokio_util::sync::CancellationToken;

use crate::Context;

#[post("/smarther_bridge/{id}")]
async fn process(path: web::Path<String>, context: Data<(Vec<String>, Sender<ModuleStatus>)>, payload: web::Json<ModuleStatus>) -> &'static str {
    let plant_id = path.into_inner();
    let is_active_plant = context.0.iter().any(|sub| sub == &plant_id);
    if !is_active_plant {
        return "Plant not active";
    }

    info!("Received status update for plant {}", plant_id);

    let tx = context.1.clone();
    if tx.send(payload.0).await.is_err() {
        error!("Failed to send status update to MQTT handler");
    }
    "OK"
}

pub(crate) async fn webhook_handler(context: &Context, cancellation_token: CancellationToken) {
    // Try to subscribe
    if context.configuration.webhook_endpoint.is_none() {
        warn!("Webhook endpoint not configured, skipping webhook handler");
        return;
    }

    tokio::join!(
        handle_subscriptions(context, cancellation_token.clone()),
        http_server(context, cancellation_token.clone())
    );
}

async fn handle_subscriptions(context: &Context, cancellation_token: CancellationToken) {
    let mut active_subscriptions = clear_active_subscriptions(context, None).await;

    if context.refresh_token_if_needed().await.is_err() {
        error!("Failed to refresh token");
        return;
    }

    let client = SmartherApi::default();
    let auth_request = client.with_authorization(context.auth_info.borrow().clone());
    if auth_request.is_err() {
        error!("Failed to create authorized client");
        return;
    }

    let client = auth_request.unwrap();
    for plant in &context.topology_cache.plants {
        let endpoint = context.configuration.webhook_endpoint.clone().unwrap();
        let plant_id = plant.id.clone();
        let endpoint_url = format!("{endpoint}/smarther_bridge/{plant_id}");
        let subscription_info = client.register_webhook(&plant_id, endpoint_url).await;
        if subscription_info.is_err() {
            error!("Failed to register webhook for plant {}: {}", plant_id, subscription_info.err().unwrap());
            continue;
        }
        let mut subscription = subscription_info.unwrap();
        subscription.plant_id = Some(plant_id);
        active_subscriptions.push(subscription);
    }

    if active_subscriptions.is_empty() {
        error!("Failed to register any webhook");
        return;
    }

    info!("Registered webhooks for {} plants", active_subscriptions.len());

    //Wait for end
    cancellation_token.cancelled().await;

    info!("Unregistering webhooks...");

    //Remove all subscriptions
    clear_active_subscriptions(context, Some(active_subscriptions)).await;
}

async fn clear_active_subscriptions(context: &Context, active_subscriptions: Option<Vec<smarther::model::SubscriptionInfo>>) -> Vec<smarther::model::SubscriptionInfo> {
    if context.refresh_token_if_needed().await.is_err() {
        error!("Failed to refresh token");
        return vec!();
    }

    let client = SmartherApi::default();
    let auth_request = client.with_authorization(context.auth_info.borrow().clone());
    if auth_request.is_err() {
        error!("Failed to create authorized client");
        return vec!();
    }

    let client = auth_request.unwrap();
    //FIXME: Right now we cancel all subscriptions, even if they are not related to this bridge
    let active_subscriptions = match active_subscriptions {
        Some(subscriptions) => subscriptions,
        None => {
            if let Ok(subscriptions) = client.get_webhooks().await {
                subscriptions
            } else {
                vec!()
            }
        }
    };

    let mut remaining_subscriptions = vec!();
    for subscription in &active_subscriptions {
        if let Some(plant_id) = &subscription.plant_id {
            let result = client.unregister_webhook(plant_id, &subscription.subscription_id).await;
            if result.is_err() {
                error!("Failed to unregister webhook {}: {}", &subscription.subscription_id, result.err().unwrap());
                remaining_subscriptions.push(subscription.clone());
            }
        }
    }

    remaining_subscriptions
}

async fn http_server(context: &Context, cancellation_token: CancellationToken) {
    //Wait for events
    let active_plants: Vec<String> = context.topology_cache.plants.iter().map(|plant| plant.id.clone()).collect();
    let sender = context.status_updates.0.clone();
    let listen_host: &str = &context.configuration.listen_host;
    let listen_port: u16 = context.configuration.listen_port;
    info!("Starting webhook server on {}:{}", listen_host, listen_port);

    let json_cfg = web::JsonConfig::default()
        .error_handler(|err, _req| {
            debug!("Failed to parse JSON: {}", err);
            error::InternalError::from_response(err, HttpResponse::Conflict().into()).into()
        });

    if let Ok(server) = HttpServer::new(move || {
        App::new()
            .app_data(Data::new((active_plants.clone(), sender.clone())))
            .app_data(json_cfg.clone())
            .service(process)
    })
    .bind((listen_host, listen_port)) {
        tokio::select! {
            _ = cancellation_token.cancelled() => {},
            _ = server.run() => {}
        }

        cancellation_token.cancel();
    }
}