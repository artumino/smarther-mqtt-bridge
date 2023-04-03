use std::time::Duration;

use bytes::Bytes;
use log::{info, error, warn};
use rumqttc::{MqttOptions, Event::Incoming, Publish, Packet, QoS};
use smarther::{model::{SetStatusRequest, TimedMeasurement, Measurement, ThermostatFunction, ThermostatMode, ThermostatStatus}, SmartherApi};
use tokio_util::sync::CancellationToken;
use anyhow::anyhow;

use crate::Context;

pub(crate) async fn mqtt_handler(context: &Context, cancellation_token: CancellationToken) {
    let configuration = &context.configuration;
    let mut options = MqttOptions::new("smarther-mqtt-bridge", configuration.mqtt_broker.clone(), configuration.mqtt_port);
    options.set_credentials(configuration.mqtt_username.clone(), configuration.mqtt_password.clone());
    let (mqtt_client, mut mqtt_loop)  = rumqttc::AsyncClient::new(options, 100);

    // Handle subscriptions for the current plant topology
    for plant in &context.topology_cache.plants {
        for module in &plant.modules {
            let device_topic = format!("{}/{}/{}/set_status", &configuration.mqtt_base_topic, &plant.id, &module.id);
            mqtt_client.subscribe(device_topic, rumqttc::QoS::AtLeastOnce).await.unwrap();
        }
    }

    tokio::select! {
        _ = cancellation_token.cancelled() => {},
        _ = mqtt_command_handler(context, &mut mqtt_loop) => {},
        _ = mqtt_status_change_handler(context, mqtt_client) => {}
    }
}

async fn try_update_plant_status(context: &Context, topic: &str, payload: &Bytes) -> anyhow::Result<()> {
    let topic_parts: Vec<&str> = topic.split('/').collect();
    if topic_parts.len() == 4 && topic_parts[3] == "set_status" {
        let plant_id = topic_parts[1];
        let module_id = topic_parts[2];
        let payload = String::from_utf8(payload.to_vec())?;
        let status_change_request: SetStatusRequest = serde_json::from_str(&payload)?;

        context.refresh_token_if_needed().await?;

        let client = SmartherApi::default();
        let auth_info = context.auth_info.borrow().clone();
        let client = client.with_authorization(auth_info)?;

        info!("Setting status for plant {} module {} to {:?}", plant_id, module_id, status_change_request);
        client.set_device_status(plant_id, module_id, status_change_request).await?;
    }
    Ok(())
}

async fn mqtt_command_handler(context: &Context, mqtt_loop: &mut rumqttc::EventLoop) {
    loop {
        let mut mqtt_event = mqtt_loop.poll().await;
        while let Ok(event) = &mqtt_event {
            if let Incoming(Packet::Publish(Publish { topic, payload, .. })) = event {
               if let Err(err) = try_update_plant_status(context, topic, payload).await {
                   error!("Error while updating plant status: {}", err);
               }
            }

            mqtt_event = mqtt_loop.poll().await;
        }
        // Reconnect timeout
        warn!("MQTT connection lost, reconnecting in 5 seconds...");
        if let Err(err) = &mqtt_event {
            warn!("MQTT Reported Error: {}", err);
        }
        tokio::time::sleep(Duration::from_millis(5000)).await;
    }
}

#[derive(Debug, Serialize)]
struct MeasurementSummary {
    temperature: Option<TimedMeasurement>,
    humidity: Option<TimedMeasurement>,
    set_point: Option<Measurement>,
    mode: ThermostatMode,
    function: ThermostatFunction,
    time: String,
    activation_time: Option<String>
}

async fn try_parse_and_publish_status(context: &Context, status: &ThermostatStatus, mqtt_client: &rumqttc::AsyncClient) -> anyhow::Result<()> {
    let sender_details = status.sender.as_ref().ok_or(anyhow!("No sender details found"))?;
    let plant_details = sender_details.plant.as_ref().ok_or(anyhow!("No plant details found"))?;


    let device_status_topic = format!("{}/{}/{}/status", &context.configuration.mqtt_base_topic, plant_details.id, plant_details.module.id);
    
    let last_temperature = status.thermometer.as_ref().and_then(|inst| inst.last_measurement());
    let last_pressure = status.hygrometer.as_ref().and_then(|inst| inst.last_measurement());
    let status_summary = MeasurementSummary {
        temperature: last_temperature.cloned(),
        humidity: last_pressure.cloned(),
        set_point: status.set_point.clone(),
        mode: status.mode.clone(),
        function: status.function.clone(),
        time: status.time.to_rfc3339(),
        activation_time: status.activation_time.map(|t| t.to_rfc3339())
    };

    mqtt_client.publish(device_status_topic, QoS::AtLeastOnce, false, serde_json::to_string(&status_summary)?).await?;
    Ok(())
}

async fn mqtt_status_change_handler(context: &Context, mqtt_client: rumqttc::AsyncClient) {
    while let Ok(status_update) = context.status_updates.1.recv().await {
        for thermostat_status in status_update.chronothermostats {
            if let Err(err) = try_parse_and_publish_status(context, &thermostat_status, &mqtt_client).await {
                error!("Error while parsing and publishing status: {}", err);
            }
        }
    }
}