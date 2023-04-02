use actix_web::{post, web::{Data, self}, HttpServer, App};
use async_channel::Sender;
use log::{error, warn};
use smarther::{model::{ModuleStatus, SubscriptionInfo}, SmartherApi};
use tokio_util::sync::CancellationToken;

use crate::Context;

#[post("/smarther_bridge/{id}")]
async fn process(path: web::Path<String>, context: Data<(Vec<String>, Vec<SubscriptionInfo>, Sender<ModuleStatus>)>, payload: web::Json<ModuleStatus>) -> &'static str {
    let plant_id = path.into_inner();
    let is_active_plant = context.0.iter().any(|sub| sub == &plant_id);
    if !is_active_plant {
        return "Plant not active";
    }

    let tx = context.2.clone();
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

    let sender = context.status_updates.0.clone();
    let mut active_subscriptions = context.active_subscriptions.clone();

    if active_subscriptions.is_empty() {
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
            active_subscriptions.push(subscription_info.unwrap());
        }

        if active_subscriptions.is_empty() {
            error!("Failed to register any webhook");
            return;
        }

        std::fs::write(&context.subscriptions_file, serde_json::to_string_pretty(&active_subscriptions).unwrap()).unwrap();
    }

    //Wait for events
    let active_plants: Vec<String> = context.topology_cache.plants.iter().map(|plant| plant.id.clone()).collect();
    let cloned_subscriptions = active_subscriptions.clone();
    if let Ok(server) = HttpServer::new(move || {
        App::new()
            .app_data(Data::new((active_plants.clone(), cloned_subscriptions.clone(), sender.clone())))
            .service(process)
    })
    .bind(("localhost", 23785)) {
        tokio::select! {
            _ = cancellation_token.cancelled() => {},
            _ = server.run() => {}
        }
    }

    //Remove all subscriptions
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
    for subscription in &active_subscriptions {
        if let Some(plant_id) = &subscription.plant_id {
            let result = client.unregister_webhook(plant_id, &subscription.subscription_id).await;
            if result.is_err() {
                error!("Failed to unregister webhook {}: {}", &subscription.subscription_id, result.err().unwrap());
            }
        }
    }
}