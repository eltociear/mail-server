/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::hash::{BuildHasher, Hash, Hasher};

use ahash::{AHashMap, AHashSet, AHasher, RandomState};
use imap_proto::{
    protocol::{expunge, select::Exists, Sequence},
    StatusResponse,
};
use jmap_proto::types::{collection::Collection, property::Property};
use store::{
    roaring::RoaringBitmap,
    write::{assert::HashedValue, now, BatchBuilder, ToBitmaps, F_VALUE},
    Deserialize, Serialize,
};
use utils::codec::leb128::{Leb128Iterator, Leb128Vec};

use crate::core::ImapId;

use super::{MailboxId, MailboxState, NextMailboxState, SelectedMailbox, SessionData};

#[derive(Debug)]
struct UidMap {
    uid_next: u32,
    uid_validity: u32,
    hash: u64,
    items: Vec<Uid>,
}

#[derive(Debug)]
struct Uid {
    uid: u32,
    id: u32,
    received: u32,
}

struct UidMapBuilder {
    message_ids: RoaringBitmap,
    hasher: AHasher,
    id_list: Vec<(u32, u32)>,
}

impl SessionData {
    pub async fn fetch_messages(&self, mailbox: &MailboxId) -> crate::op::Result<MailboxState> {
        let mut try_count = 0;

        loop {
            // Deserialize mailbox data
            let uid_map = self
                .jmap
                .get_property::<HashedValue<UidMap>>(
                    mailbox.account_id,
                    Collection::Mailbox,
                    mailbox.mailbox_id.unwrap_or(u32::MAX),
                    Property::EmailIds,
                )
                .await?;

            // Obtain current state
            let modseq = self
                .jmap
                .store
                .get_last_change_id(mailbox.account_id, Collection::Email)
                .await
                .map_err(|err| {
                    tracing::error!(event = "error",
                context = "store",
                account_id = mailbox.account_id,
                collection = ?Collection::Email,
                error = ?err,
                "Failed to obtain state");
                    StatusResponse::database_failure()
                })?;

            // Obtain message ids
            let message_ids = if let Some(mailbox_id) = mailbox.mailbox_id {
                self.jmap
                    .get_tag(
                        mailbox.account_id,
                        Collection::Email,
                        Property::MailboxIds,
                        mailbox_id,
                    )
                    .await?
                    .unwrap_or_default()
            } else {
                self.jmap
                    .get_document_ids(mailbox.account_id, Collection::Email)
                    .await?
                    .unwrap_or_default()
            };

            // Obtain message data
            let (id_list, id_list_hash) = if !message_ids.is_empty() {
                let uid_builder = self
                    .jmap
                    .store
                    .index_values(
                        UidMapBuilder {
                            id_list: Vec::with_capacity(message_ids.len() as usize),
                            message_ids,
                            hasher: RandomState::with_seeds(
                                0xaf1f2242106c64b3,
                                0x60ca4cfb4b3ed0ce,
                                0xc7dbc0bb615e82b3,
                                0x520ad065378daf88,
                            )
                            .build_hasher(),
                        },
                        mailbox.account_id,
                        Collection::Email,
                        Property::ReceivedAt,
                        true,
                        |uid_builder, message_id, bytes| {
                            if uid_builder.message_ids.remove(message_id) {
                                let received = (u64::deserialize(bytes)? & u32::MAX as u64) as u32;
                                uid_builder.id_list.push((message_id, received));
                                message_id.hash(&mut uid_builder.hasher);
                                received.hash(&mut uid_builder.hasher);
                                Ok(!uid_builder.message_ids.is_empty())
                            } else {
                                Ok(true)
                            }
                        },
                    )
                    .await
                    .map_err(|err| {
                        tracing::error!(event = "error",
                    context = "store",
                    account_id = mailbox.account_id,
                    collection = ?Collection::Email,
                    error = ?err,
                    "Failed to obtain message data");
                        StatusResponse::database_failure()
                    })?;
                (uid_builder.id_list, uid_builder.hasher.finish())
            } else {
                (Vec::new(), 0)
            };

            // Build mailboxdata
            if let Some(mut uid_map) = uid_map {
                if uid_map.inner.hash != id_list_hash {
                    let mut id_list_map = id_list.iter().cloned().collect::<AHashSet<_>>();
                    let mut items = Vec::with_capacity(uid_map.inner.items.len());

                    for item in uid_map.inner.items {
                        if id_list_map.remove(&(item.id, item.received)) {
                            items.push(item);
                        }
                    }

                    for (id, received) in id_list_map {
                        items.push(Uid {
                            uid: uid_map.inner.uid_next,
                            id,
                            received,
                        });

                        uid_map.inner.uid_next += 1;
                    }

                    uid_map.inner.items = items;
                    uid_map.inner.hash = id_list_hash;

                    // Save updated uid map
                    let mut batch = BatchBuilder::new();
                    batch
                        .with_account_id(mailbox.account_id)
                        .with_collection(Collection::Mailbox)
                        .update_document(mailbox.mailbox_id.unwrap_or(u32::MAX))
                        .assert_value(Property::EmailIds, &uid_map)
                        .value(Property::EmailIds, &uid_map.inner, F_VALUE);

                    match self.jmap.store.write(batch.build()).await {
                        Ok(_) => (),
                        Err(store::Error::AssertValueFailed) if try_count < 3 => {
                            try_count += 1;
                            continue;
                        }
                        Err(err) => {
                            tracing::error!(event = "error",
                                            context = "store",
                                            account_id = mailbox.account_id,
                                            collection = ?Collection::Mailbox,
                                            error = ?err,
                                            "Failed to store uid map");
                            return Err(StatusResponse::database_failure());
                        }
                    }
                }

                let uid_map = uid_map.inner;
                let mut id_to_imap = AHashMap::with_capacity(uid_map.items.len());
                let mut uid_to_id = AHashMap::with_capacity(uid_map.items.len());
                let mut uid_max = 0;

                for (seqnum, item) in uid_map.items.into_iter().enumerate() {
                    id_to_imap.insert(
                        item.id,
                        ImapId {
                            uid: item.uid,
                            seqnum: (seqnum + 1) as u32,
                        },
                    );
                    uid_to_id.insert(item.uid, item.id);
                    uid_max = item.uid;
                }

                return Ok(MailboxState {
                    uid_next: uid_map.uid_next,
                    uid_validity: uid_map.uid_validity,
                    total_messages: id_to_imap.len(),
                    id_to_imap,
                    uid_to_id,
                    uid_max,
                    modseq,
                    next_state: None,
                });
            } else {
                let uid_next = (id_list.len() + 1) as u32;
                let uid_validity = now() as u32 ^ mailbox.mailbox_id.unwrap_or(0);
                let mut id_to_imap = AHashMap::with_capacity(id_list.len());
                let mut uid_to_id = AHashMap::with_capacity(id_list.len());
                let mut uids = Vec::with_capacity(id_list.len());
                let mut uid_map = UidMap {
                    uid_next,
                    uid_validity,
                    hash: id_list_hash,
                    items: Vec::with_capacity(id_list.len()),
                };

                for (uid, (id, received)) in id_list.into_iter().enumerate() {
                    let uid = (uid + 1) as u32;
                    id_to_imap.insert(id, ImapId { uid, seqnum: uid });
                    uid_to_id.insert(uid, id);
                    uids.push(uid);
                    uid_map.items.push(Uid { uid, id, received });
                }

                // Store uid map
                let mut batch = BatchBuilder::new();
                batch
                    .with_account_id(mailbox.account_id)
                    .with_collection(Collection::Mailbox)
                    .update_document(mailbox.mailbox_id.unwrap_or(u32::MAX))
                    .value(Property::EmailIds, &uid_map, F_VALUE);
                self.jmap.store.write(batch.build()).await.map_err(|err| {
                    tracing::error!(event = "error",
                    context = "store",
                    account_id = mailbox.account_id,
                    collection = ?Collection::Mailbox,
                    error = ?err,
                    "Failed to store uid map");
                    StatusResponse::database_failure()
                })?;

                return Ok(MailboxState {
                    uid_next,
                    uid_validity,
                    total_messages: uids.len(),
                    id_to_imap,
                    uid_to_id,
                    uid_max: uid_next.saturating_sub(1),
                    modseq,
                    next_state: None,
                });
            }
        }
    }

    pub async fn synchronize_messages(
        &self,
        mailbox: &SelectedMailbox,
    ) -> crate::op::Result<Option<u64>> {
        // Obtain current modseq
        let modseq = self.get_modseq(mailbox.id.account_id).await?;
        if mailbox.state.lock().modseq != modseq {
            // Synchronize messages
            let new_state = self.fetch_messages(&mailbox.id).await?;
            let mut current_state = mailbox.state.lock();

            // Add missing uids
            let mut deletions = current_state
                .next_state
                .take()
                .map(|state| state.deletions)
                .unwrap_or_default();
            let mut id_to_imap = AHashMap::with_capacity(current_state.id_to_imap.len());
            for (id, imap_id) in std::mem::take(&mut current_state.id_to_imap) {
                if !new_state.uid_to_id.contains_key(&imap_id.uid) {
                    // Add to deletions
                    deletions.push(imap_id);

                    // Invalidate entries
                    current_state.uid_to_id.remove(&imap_id.uid);
                } else {
                    id_to_imap.insert(id, imap_id);
                }
            }
            current_state.id_to_imap = id_to_imap;

            // Update state
            current_state.modseq = new_state.modseq;
            current_state.next_state = Some(Box::new(NextMailboxState {
                next_state: new_state,
                deletions,
            }));
        }

        Ok(modseq)
    }

    pub async fn write_mailbox_changes(
        &self,
        mailbox: &SelectedMailbox,
        is_qresync: bool,
    ) -> crate::op::Result<Option<u64>> {
        // Resync mailbox
        let modseq = self.synchronize_messages(mailbox).await?;
        let mut buf = Vec::new();
        {
            let mut current_state = mailbox.state.lock();
            if let Some(next_state) = current_state.next_state.take() {
                if !next_state.deletions.is_empty() {
                    let mut ids = next_state
                        .deletions
                        .into_iter()
                        .map(|id| if is_qresync { id.uid } else { id.seqnum })
                        .collect::<Vec<u32>>();
                    ids.sort_unstable();
                    expunge::Response { is_qresync, ids }.serialize_to(&mut buf);
                }
                if !buf.is_empty()
                    || next_state
                        .next_state
                        .uid_max
                        .saturating_sub(current_state.uid_max)
                        > 0
                {
                    Exists {
                        total_messages: next_state.next_state.total_messages,
                    }
                    .serialize(&mut buf);
                }
                *current_state = next_state.next_state;
            }
        }
        if !buf.is_empty() {
            self.write_bytes(buf).await;
        }

        Ok(modseq)
    }

    pub async fn get_modseq(&self, account_id: u32) -> crate::op::Result<Option<u64>> {
        // Obtain current modseq
        if let Ok(modseq) = self
            .jmap
            .store
            .get_last_change_id(account_id, Collection::Email)
            .await
        {
            Ok(modseq)
        } else {
            tracing::error!(parent: &self.span,
                event = "error",
                context = "store",
                account_id = account_id,
                collection = ?Collection::Email,
                "Failed to obtain modseq");
            Err(StatusResponse::database_failure())
        }
    }
}

impl SelectedMailbox {
    pub async fn sequence_to_ids(
        &self,
        sequence: &Sequence,
        is_uid: bool,
    ) -> crate::op::Result<AHashMap<u32, ImapId>> {
        if !sequence.is_saved_search() {
            let mut ids = AHashMap::new();
            let state = self.state.lock();
            if state.id_to_imap.is_empty() {
                return Ok(ids);
            }

            if is_uid {
                for (id, imap_id) in &state.id_to_imap {
                    if sequence.contains(imap_id.uid, state.uid_max) {
                        ids.insert(*id, *imap_id);
                    }
                }
            } else {
                for (id, imap_id) in &state.id_to_imap {
                    if sequence.contains(imap_id.seqnum, state.total_messages as u32) {
                        ids.insert(*id, *imap_id);
                    }
                }
            }

            Ok(ids)
        } else {
            let saved_ids = self
                .get_saved_search()
                .await
                .ok_or_else(|| StatusResponse::no("No saved search found."))?;
            let mut ids = AHashMap::with_capacity(saved_ids.len());
            let state = self.state.lock();

            for imap_id in saved_ids.iter() {
                if let Some(id) = state.uid_to_id.get(&imap_id.uid) {
                    ids.insert(*id, *imap_id);
                }
            }

            Ok(ids)
        }
    }

    pub async fn sequence_expand_missing(&self, sequence: &Sequence, is_uid: bool) -> Vec<u32> {
        let mut deleted_ids = Vec::new();
        if !sequence.is_saved_search() {
            let state = self.state.lock();
            if is_uid {
                for uid in sequence.expand(state.uid_max) {
                    if !state.uid_to_id.contains_key(&uid) {
                        deleted_ids.push(uid);
                    }
                }
            } else {
                for seqnum in sequence.expand(state.total_messages as u32) {
                    if seqnum > state.total_messages as u32 {
                        deleted_ids.push(seqnum);
                    }
                }
            }
        } else if let Some(saved_ids) = self.get_saved_search().await {
            let state = self.state.lock();
            for id in saved_ids.iter() {
                if !state.uid_to_id.contains_key(&id.uid) {
                    deleted_ids.push(if is_uid { id.uid } else { id.seqnum });
                }
            }
        }
        deleted_ids.sort_unstable();
        deleted_ids
    }
}

impl Serialize for &UidMap {
    fn serialize(self) -> Vec<u8> {
        let mut buf = Vec::with_capacity((self.items.len() + 2) * std::mem::size_of::<u64>());
        buf.push_leb128(self.items.len());
        buf.push_leb128(self.uid_next);
        buf.extend_from_slice(self.uid_validity.to_le_bytes().as_ref());
        buf.extend_from_slice(self.hash.to_le_bytes().as_ref());

        let mut last_uid = 0;
        for item in &self.items {
            if last_uid + 1 != item.uid {
                buf.push(0);
                buf.push_leb128(item.uid);
            }

            buf.push_leb128(item.id + 1);
            buf.extend_from_slice(item.received.to_le_bytes().as_ref());
            last_uid = item.uid;
        }

        buf
    }
}

impl UidMap {
    fn deserialize_(bytes: &[u8]) -> Option<Self> {
        let mut buf_u32 = [0u8; std::mem::size_of::<u32>()];
        let mut buf_u64 = [0u8; std::mem::size_of::<u64>()];

        let mut bytes = bytes.iter();
        let items_len: usize = bytes.next_leb128()?;
        let uid_next: u32 = bytes.next_leb128()?;
        buf_u32
            .iter_mut()
            .try_for_each(|b| bytes.next().map(|v| *b = *v))?;
        buf_u64
            .iter_mut()
            .try_for_each(|b| bytes.next().map(|v| *b = *v))?;
        let mut uid_map = UidMap {
            uid_next,
            uid_validity: u32::from_le_bytes(buf_u32),
            hash: u64::from_le_bytes(buf_u64),
            items: Vec::with_capacity(items_len),
        };
        let mut next_uid: u32 = 1;
        for _ in 0..items_len {
            let mut id: u32 = bytes.next_leb128()?;
            if id == 0 {
                next_uid = bytes.next_leb128()?;
                id = bytes.next_leb128()?;
            }
            buf_u32
                .iter_mut()
                .try_for_each(|b| bytes.next().map(|v| *b = *v))?;
            uid_map.items.push(Uid {
                uid: next_uid,
                id: id - 1,
                received: u32::from_le_bytes(buf_u32),
            });
            next_uid += 1;
        }

        uid_map.into()
    }
}

impl Deserialize for UidMap {
    fn deserialize(bytes: &[u8]) -> store::Result<Self> {
        Self::deserialize_(bytes).ok_or(store::Error::InternalError(
            "Failed to deserialize uid map".to_string(),
        ))
    }
}

impl ToBitmaps for &UidMap {
    fn to_bitmaps(&self, _: &mut Vec<store::write::Operation>, _: u8, _: bool) {
        unreachable!()
    }
}
