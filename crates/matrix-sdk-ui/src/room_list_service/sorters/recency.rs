// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::Ordering;

use super::{RoomListItem, Sorter};

type GetScores = fn(&RoomListItem, &RoomListItem) -> (Option<Score>, Option<Score>);

fn cmp(scores: GetScores, left: &RoomListItem, right: &RoomListItem) -> Ordering {
    let (a, b) = scores(left, right);
    cmp_impl(a, b)
}

fn cmp_impl(a: Option<Score>, b: Option<Score>) -> Ordering {
    match (a, b) {
        (Some(left), Some(right)) => left.cmp(&right).reverse(),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Create a new sorter that will sort two [`RoomListItem`] by their
/// [`RoomInfo::recency_stamp`]. The `Room` with the highest stamp comes first.
///
/// Not the latest event's timestamp: a room only has one once it is subscribed
/// to and decrypted, so rooms would move as they scroll into view.
///
/// [`RoomInfo::recency_stamp`]: matrix_sdk_base::RoomInfo::recency_stamp
pub fn new_sorter() -> impl Sorter {
    |left, right| -> Ordering { cmp(extract_scores, left, right) }
}

type Score = u64;

fn extract_scores(left: &RoomListItem, right: &RoomListItem) -> (Option<Score>, Option<Score>) {
    (left.cached_recency_stamp.map(Into::into), right.cached_recency_stamp.map(Into::into))
}

#[cfg(test)]
mod tests {
    use matrix_sdk::{
        RoomRecencyStamp,
        latest_events::{LatestEventValue, LocalLatestEventValue, RemoteLatestEventValue},
        store::SerializableEventContent,
        test_utils::mocks::MatrixMockServer,
    };
    use matrix_sdk_base::RoomInfoNotableUpdateReasons;
    use matrix_sdk_test::async_test;
    use proptest::prelude::*;
    use ruma::{
        MilliSecondsSinceUnixEpoch,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        room_id,
        serde::Raw,
    };
    use serde_json::json;

    use super::{super::super::filters::new_rooms, *};

    fn none() -> LatestEventValue {
        LatestEventValue::None
    }

    fn remote(origin_server_ts: u64) -> LatestEventValue {
        LatestEventValue::Remote(RemoteLatestEventValue::from_plaintext(
            Raw::from_json_string(
                json!({
                    "content": RoomMessageEventContent::text_plain("raclette"),
                    "type": "m.room.message",
                    "event_id": "$ev0",
                    "room_id": "!r0",
                    "origin_server_ts": origin_server_ts,
                    "sender": "@mnt_io:matrix.org",
                })
                .to_string(),
            )
            .unwrap(),
        ))
    }

    fn local_is_sending(origin_server_ts: u32) -> LatestEventValue {
        LatestEventValue::LocalIsSending(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(origin_server_ts.into()),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        })
    }

    fn local_cannot_be_sent(origin_server_ts: u32) -> LatestEventValue {
        LatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(origin_server_ts.into()),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        })
    }

    async fn set_latest_event_value(room: &mut RoomListItem, latest_event_value: LatestEventValue) {
        room.update_room_info(|mut info| {
            info.set_latest_event(latest_event_value);
            (info, RoomInfoNotableUpdateReasons::LATEST_EVENT)
        })
        .await;
        room.refresh_cached_data();
    }

    async fn set_recency_stamp(room: &mut RoomListItem, recency_stamp: RoomRecencyStamp) {
        room.update_room_info(|mut info| {
            info.update_recency_stamp(recency_stamp);
            (info, RoomInfoNotableUpdateReasons::RECENCY_STAMP)
        })
        .await;
        room.refresh_cached_data();
    }

    #[async_test]
    async fn test_extract_scores_ignores_the_latest_event() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [mut room_a, mut room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        set_recency_stamp(&mut room_a, 1.into()).await;
        set_recency_stamp(&mut room_b, 2.into()).await;

        for latest_event_value_a in
            [none(), remote(30), local_is_sending(30), local_cannot_be_sent(30)]
        {
            for latest_event_value_b in
                [none(), remote(40), local_is_sending(40), local_cannot_be_sent(40)]
            {
                set_latest_event_value(&mut room_a, latest_event_value_a.clone()).await;
                set_latest_event_value(&mut room_b, latest_event_value_b).await;

                assert_eq!(extract_scores(&room_a, &room_b), (Some(1), Some(2)));
            }
        }
    }

    #[async_test]
    async fn test_with_two_scores() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has an older recency stamp than `room_b`.
        {
            // `room_a` has a smaller score than `room_b`, i.e. it must come
            // after `room_b`.
            assert_eq!(
                cmp(|_left, _right| (Some(1), Some(2)), &room_a, &room_b),
                Ordering::Greater
            );
        }

        // `room_b` has a smaller score than `room_a`.
        {
            // `room_a` has a greater score than `room_b`, i.e. it must come
            // before `room_b`.
            assert_eq!(cmp(|_left, _right| (Some(2), Some(1)), &room_a, &room_b), Ordering::Less);
        }

        // `room_a` has the same score than `room_b`.
        {
            assert_eq!(cmp(|_left, _right| (Some(1), Some(1)), &room_a, &room_b), Ordering::Equal);
        }
    }

    #[async_test]
    async fn test_with_one_score() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has a score, but `room_b` has none: `room_a` must come
        // before `room_b`.
        {
            assert_eq!(cmp(|_left, _right| (Some(1), None), &room_a, &room_b), Ordering::Less);
        }

        // `room_a` has no score, but `room_b` has one: `room_a` must come after
        // `room_b`.
        {
            assert_eq!(cmp(|_left, _right| (None, Some(1)), &room_a, &room_b), Ordering::Greater);
        }
    }

    #[async_test]
    async fn test_with_zero_score() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` and `room_b` has no score: they are equal.
        {
            assert_eq!(cmp(|_left, _right| (None, None), &room_a, &room_b), Ordering::Equal);
        }
    }

    prop_compose! {
        fn arb_score()(score in any::<Option<u64>>()) -> Option<Score> {
            score
        }
    }

    // Property tests to ensure that our comparison implementation for the
    // `Score` is total[1]. If it wasn't so, a call to `sort_by()` with the
    // given `cmp()` method could panic.
    //
    // [1]: https://en.wikipedia.org/wiki/Total_order
    proptest! {
        // Test that the ordering is reflexive, that is, each `Score` needs to
        // be equal to itself. a <= a.
        #![proptest_config(ProptestConfig::with_cases(10_000))]
        #[test]
        fn test_cmp_reflexive(score in arb_score()) {
            let result = cmp_impl(score, score);
            assert!(result == Ordering::Less || result == Ordering::Equal);
        }

        // Test that the ordering is transitive, that is, if a <= b and b <= c then a <= c.
        #[test]
        fn test_cmp_transitive(a in arb_score(), b in arb_score(), c in arb_score()) {
            use Ordering::*;

            let a_b_comparison = cmp_impl(a, b);
            let b_c_comparison = cmp_impl(b, c);
            let a_c_comparison = cmp_impl(a, c);

            if matches!(a_b_comparison, Less | Equal) && matches!(b_c_comparison, Less | Equal) {
                assert!(a_c_comparison == Less || a_c_comparison == Equal);
            }
        }

        // Test that the ordering is antisymetric, that is, if a <= b and b <= a then a = b.
        #[test]
        fn test_cmp_antisymetric(a in arb_score(), b in arb_score()) {
            use Ordering::*;

            let a_b_comparison = cmp_impl(a, b);
            let b_a_comparison = cmp_impl(b, a);

            eprintln!("a.cmp(b) = {a_b_comparison:?}");
            eprintln!("b.cmp(a) = {b_a_comparison:?}");

            if matches!(a_b_comparison, Less | Equal) && matches!(b_a_comparison, Less | Equal) {
                assert!(a == b);
            }
        }

        // Test that the ordering is total, that is, we have either a <= b or b <= a.
        #[test]
        fn test_cmp_reverse(a in arb_score(), b in arb_score()) {
            use Ordering::*;

            let a_b_comparison = cmp_impl(a, b);
            let b_a_comparison = cmp_impl(b, a);

            if a_b_comparison == Less {
                assert_eq!(b_a_comparison, Greater);
            } else if a_b_comparison == Greater {
                assert_eq!(b_a_comparison, Less);
            } else {
                assert_eq!(a_b_comparison, Equal);
                assert_eq!(b_a_comparison, Equal);
            }
        }
    }
}
