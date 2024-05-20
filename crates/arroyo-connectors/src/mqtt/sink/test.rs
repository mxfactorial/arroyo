use arrow::array::{RecordBatch, StringArray};
use std::collections::HashMap;
use std::sync::Arc;

use crate::mqtt::{create_connection, MqttConfig, Tls};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arroyo_operator::context::ArrowContext;
use arroyo_operator::operator::ArrowOperator;
use arroyo_rpc::df::ArroyoSchema;
use arroyo_rpc::{
    formats::{Format, JsonFormat},
    var_str::VarStr,
};
use arroyo_types::get_test_task_info;
use parquet::data_type::AsBytes;
use rumqttc::{
    v5::{mqttbytes::QoS, Event, Incoming},
    Outgoing,
};
use serde::Deserialize;
use tokio::sync::mpsc::channel;

use super::MqttSinkFunc;

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![Field::new(
        "value",
        DataType::Utf8,
        false,
    )]))
}

#[derive(Deserialize)]
struct TestData {
    value: String,
}

pub struct MqttTopicTester {
    topic: String,
    port: u16,
    ca: Option<String>,
    cert: Option<String>,
    key: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

impl MqttTopicTester {
    fn get_config(&self) -> MqttConfig {
        MqttConfig {
            url: format!("tcp://localhost:{}", self.port),
            client_prefix: Some("test".to_string()),
            username: self.username.as_ref().map(|u| VarStr::new(u.clone())),
            password: self.password.as_ref().map(|p| VarStr::new(p.clone())),
            tls: Some(Tls {
                ca: self.ca.as_ref().map(|ca| VarStr::new(ca.clone())),
                cert: self.cert.as_ref().map(|ca| VarStr::new(ca.clone())),
                key: self.key.as_ref().map(|ca| VarStr::new(ca.clone())),
            }),
        }
    }

    async fn get_client(&self) -> (rumqttc::v5::AsyncClient, rumqttc::v5::EventLoop) {
        let config = self.get_config();
        create_connection(&config, 0).expect("Failed to create connection")
    }

    async fn get_sink_with_writes(&self) -> MqttSinkWithWrites {
        let config = self.get_config();
        let mut mqtt = MqttSinkFunc::new(
            config,
            QoS::AtLeastOnce,
            self.topic.clone(),
            false,
            Format::Json(JsonFormat::default()),
        );

        let (_, control_rx) = channel(128);
        let (command_tx, _) = channel(128);

        let task_info = get_test_task_info();

        let mut ctx = ArrowContext::new(
            task_info,
            None,
            control_rx,
            command_tx,
            1,
            vec![ArroyoSchema::new_unkeyed(schema(), 0)],
            None,
            None,
            vec![vec![]],
            HashMap::new(),
        )
        .await;

        mqtt.on_start(&mut ctx).await;

        MqttSinkWithWrites { sink: mqtt, ctx }
    }
}

struct MqttSinkWithWrites {
    sink: MqttSinkFunc,
    ctx: ArrowContext,
}

#[tokio::test]
#[ignore]
async fn test_mqtt() {
    let mqtt_tester = MqttTopicTester {
        topic: "mqtt-arroyo-test-sink".to_string(),
        port: 1883,
        ca: None,
        cert: None,
        key: None,
        username: None,
        password: None,
    };

    let mut sink_with_writes = mqtt_tester.get_sink_with_writes().await;
    let (client, mut eventloop) = mqtt_tester.get_client().await;

    client
        .subscribe(&mqtt_tester.topic, QoS::AtLeastOnce)
        .await
        .unwrap();
    let start = std::time::Instant::now();

    loop {
        match eventloop.poll().await {
            Ok(Event::Outgoing(Outgoing::Subscribe(_))) => {
                break;
            }
            _ => {
                if start.elapsed().as_secs() > 5 {
                    panic!("Failed to subscribe to topic");
                }
            }
        }
    }

    for message in 1u32..200 {
        let data = StringArray::from_iter_values(vec![message.to_string()].into_iter());
        let batch = RecordBatch::try_new(schema(), vec![Arc::new(data)]).unwrap();

        sink_with_writes
            .sink
            .process_batch(batch, &mut sink_with_writes.ctx)
            .await;
    }

    let mut message = 1u32;

    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Incoming::Publish(p))) => {
                let result: TestData = serde_json::from_slice(p.payload.as_bytes()).unwrap();
                assert_eq!(
                    message.to_string(),
                    result.value,
                    "{} {:?}",
                    message,
                    String::from_utf8_lossy(p.payload.as_bytes())
                );
                message += 1;
                if message >= 200 {
                    break;
                }
            }
            Ok(_) => (),
            Err(err) => {
                panic!("Error in mqtt event loop: {:?}", err);
            }
        }
    }
}
