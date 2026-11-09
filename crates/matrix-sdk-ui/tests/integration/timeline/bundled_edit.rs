use std::time::Duration;

use matrix_sdk::{
    Client,
    encryption::EncryptionSettings,
    test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
};
use matrix_sdk_base::crypto::types::events::room::encrypted::EncryptedToDeviceEvent;
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{RoomExt, Timeline};
use ruma::{
    EventId, RoomId, RoomVersionId, device_id, event_id,
    events::{AnySyncTimelineEvent, room::message::RoomMessageEventContentWithoutRelation},
    room_id,
    serde::Raw,
    user_id,
};
use serde_json::json;

async fn set_up(room_id: &RoomId) -> (Client, Client, MatrixMockServer) {
    let alice_id = user_id!("@alice:localhost");
    let bob_id = user_id!("@bob:localhost");
    let server = MatrixMockServer::new().await;
    server.mock_crypto_endpoints_preset().await;
    let settings = EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

    let alice = server
        .client_builder_for_crypto_end_to_end(alice_id, device_id!("ALICEDEVICE"))
        .on_builder(|b| b.with_encryption_settings(settings))
        .build()
        .await;
    let bob = server
        .client_builder_for_crypto_end_to_end(bob_id, device_id!("BOBDEVICE"))
        .on_builder(|b| b.with_encryption_settings(settings))
        .build()
        .await;
    server.exchange_e2ee_identities(&alice, &bob).await;

    let f = EventFactory::new().room(room_id).sender(alice_id);
    let room = JoinedRoomBuilder::new(room_id)
        .add_state_event(f.create(alice_id, RoomVersionId::V1))
        .add_state_event(f.room_encryption());
    server
        .mock_sync()
        .ok_and_run(&alice, |b| {
            b.add_joined_room(room.clone());
        })
        .await;
    server
        .mock_sync()
        .ok_and_run(&bob, |b| {
            b.add_joined_room(room);
        })
        .await;
    server
        .mock_get_members()
        .ok(vec![f.member(alice_id).into_raw(), f.member(bob_id).into_raw()])
        .mount()
        .await;

    (alice, bob, server)
}

async fn send_raw(
    server: &MatrixMockServer,
    client: &Client,
    room_id: &RoomId,
    event_id: &EventId,
    content: serde_json::Value,
) -> (Raw<AnySyncTimelineEvent>, Raw<EncryptedToDeviceEvent>) {
    let user_id = client.user_id().unwrap();
    let (event, mock) = server.mock_room_send().ok_with_capture(event_id, user_id);
    let _send = mock.mock_once().mount_as_scoped().await;
    let (_to_device, room_key) = server.mock_capture_put_to_device(user_id).await;
    client.get_room(room_id).unwrap().send_raw("m.room.message", content).await.unwrap();
    (event.await.unwrap(), room_key.await)
}

fn edit_content(original: &EventId, body: &str) -> serde_json::Value {
    json!({
        "msgtype": "m.text",
        "body": format!("* {body}"),
        "m.new_content": { "msgtype": "m.text", "body": body },
        "m.relates_to": { "rel_type": "m.replace", "event_id": original },
    })
}

fn with_bundled_edit(
    original: &Raw<AnySyncTimelineEvent>,
    edit: &Raw<AnySyncTimelineEvent>,
) -> Raw<AnySyncTimelineEvent> {
    let mut original: serde_json::Value = original.deserialize_as().unwrap();
    original["unsigned"] = json!({ "m.relations": { "m.replace": edit } });
    Raw::new(&original).unwrap().cast_unchecked()
}

async fn sync_keys(
    server: &MatrixMockServer,
    client: &Client,
    keys: &[Raw<EncryptedToDeviceEvent>],
) {
    server
        .mock_sync()
        .ok_and_run(client, |b| {
            for key in keys {
                b.add_to_device_event(key.deserialize_as().unwrap());
            }
        })
        .await;
}

async fn body_of(timeline: &Timeline, event_id: &EventId) -> Option<String> {
    timeline
        .items()
        .await
        .iter()
        .filter_map(|item| item.as_event().cloned())
        .find(|event| event.event_id() == Some(event_id))
        .and_then(|event| event.content().as_message().map(|m| m.body().to_owned()))
}

async fn wait_for_body(timeline: &Timeline, event_id: &EventId, expected: &str) -> Option<String> {
    let mut last = None;
    for _ in 0..40 {
        last = body_of(timeline, event_id).await;
        if last.as_deref() == Some(expected) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    last
}

#[async_test]
async fn test_utd_edit_is_applied_once_its_key_arrives() {
    let room_id = room_id!("!test:localhost");
    let (original_id, edit_id) = (event_id!("$original"), event_id!("$edit"));
    let (alice, bob, server) = set_up(room_id).await;

    let (original, original_key) = send_raw(
        &server,
        &alice,
        room_id,
        original_id,
        json!({ "msgtype": "m.text", "body": "old" }),
    )
    .await;
    alice.get_room(room_id).unwrap().discard_room_key().await.unwrap();
    let (edit, edit_key) =
        send_raw(&server, &alice, room_id, edit_id, edit_content(original_id, "new")).await;

    let timeline = bob.get_room(room_id).unwrap().timeline().await.unwrap();
    let (_, _stream) = timeline.subscribe().await;

    sync_keys(&server, &bob, &[original_key]).await;
    server
        .mock_sync()
        .ok_and_run(&bob, |b| {
            b.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(original)
                    .add_timeline_event(edit),
            );
        })
        .await;
    assert_eq!(wait_for_body(&timeline, original_id, "old").await.as_deref(), Some("old"));

    sync_keys(&server, &bob, &[edit_key]).await;
    assert_eq!(wait_for_body(&timeline, original_id, "new").await.as_deref(), Some("new"));
}

#[async_test]
async fn test_bundled_utd_edit_is_applied_once_its_key_arrives() {
    let room_id = room_id!("!test:localhost");
    let (original_id, edit_id) = (event_id!("$original"), event_id!("$edit"));
    let (alice, bob, server) = set_up(room_id).await;

    let (original, original_key) = send_raw(
        &server,
        &alice,
        room_id,
        original_id,
        json!({ "msgtype": "m.text", "body": "old" }),
    )
    .await;
    alice.get_room(room_id).unwrap().discard_room_key().await.unwrap();
    let (edit, edit_key) =
        send_raw(&server, &alice, room_id, edit_id, edit_content(original_id, "new")).await;

    let timeline = bob.get_room(room_id).unwrap().timeline().await.unwrap();
    let (_, _stream) = timeline.subscribe().await;

    sync_keys(&server, &bob, &[original_key]).await;
    server
        .mock_sync()
        .ok_and_run(&bob, |b| {
            b.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(with_bundled_edit(&original, &edit))
                    .set_timeline_limited()
                    .set_timeline_prev_batch("prev_batch"),
            );
        })
        .await;
    assert_eq!(wait_for_body(&timeline, original_id, "old").await.as_deref(), Some("old"));

    sync_keys(&server, &bob, &[edit_key]).await;
    assert_eq!(wait_for_body(&timeline, original_id, "new").await.as_deref(), Some("new"));
}

#[async_test]
async fn test_bundled_edit_survives_a_limited_sync() {
    let room_id = room_id!("!test:localhost");
    let (original_id, edit_id) = (event_id!("$original"), event_id!("$edit"));
    let (alice, bob, server) = set_up(room_id).await;

    let (original, original_key) = send_raw(
        &server,
        &alice,
        room_id,
        original_id,
        json!({ "msgtype": "m.text", "body": "old" }),
    )
    .await;
    alice.get_room(room_id).unwrap().discard_room_key().await.unwrap();
    let (edit, edit_key) =
        send_raw(&server, &alice, room_id, edit_id, edit_content(original_id, "new")).await;

    let room = bob.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, _stream) = timeline.subscribe().await;

    sync_keys(&server, &bob, &[original_key, edit_key]).await;
    server
        .mock_sync()
        .ok_and_run(&bob, |b| {
            b.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(with_bundled_edit(&original, &edit))
                    .set_timeline_limited()
                    .set_timeline_prev_batch("prev_batch"),
            );
        })
        .await;
    assert_eq!(wait_for_body(&timeline, original_id, "new").await.as_deref(), Some("new"));

    let fresh = room.timeline().await.unwrap();
    assert_eq!(wait_for_body(&fresh, original_id, "new").await.as_deref(), Some("new"));
}

#[async_test]
async fn test_edit_inside_a_sync_gap_is_applied_after_backpagination() {
    let room_id = room_id!("!test:localhost");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    let alice = user_id!("@alice:localhost");
    let f = EventFactory::new().room(room_id).sender(alice);
    let (original_id, edit_id) = (event_id!("$original"), event_id!("$edit"));

    let timeline = room.timeline().await.unwrap();
    let (_, _stream) = timeline.subscribe().await;

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("old").event_id(original_id)),
        )
        .await;
    assert_eq!(wait_for_body(&timeline, original_id, "old").await.as_deref(), Some("old"));

    let edit = f
        .text_msg("* new")
        .event_id(edit_id)
        .edit(original_id, RoomMessageEventContentWithoutRelation::text_plain("new"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("later").event_id(event_id!("$later")))
                .set_timeline_limited()
                .set_timeline_prev_batch("gap_token"),
        )
        .await;

    server
        .mock_room_messages()
        .match_from("gap_token")
        .ok(RoomMessagesResponseTemplate::default().events(vec![edit]).end_token("before_edit"))
        .mount()
        .await;
    server
        .mock_room_messages()
        .match_from("before_edit")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("old").event_id(original_id)]))
        .mount()
        .await;

    for _ in 0..5 {
        if timeline.paginate_backwards(10).await.unwrap() {
            break;
        }
    }
    assert_eq!(wait_for_body(&timeline, original_id, "new").await.as_deref(), Some("new"));
}
