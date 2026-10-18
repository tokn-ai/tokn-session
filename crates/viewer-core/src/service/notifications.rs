use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
  SessionAttention, SessionHeaderInventory, ViewerService, index_catalog_source_key, index_session_key,
  indexed_session_header, locator_for_header, session_relation_attention, session_relation_index,
};
use crate::model::{SessionLocator, ViewerProvider, decode_session_key, encode_session_key};

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct NotificationWorkStats {
  pub(crate) provider_snapshots: usize,
  pub(crate) rows_loaded: usize,
}

impl ViewerService {
  #[cfg(test)]
  pub(crate) fn notification_work_stats(&self) -> NotificationWorkStats {
    *self.notification_work_stats.lock().unwrap()
  }

  /// Refresh changed rows and their canonical ancestors from one metadata
  /// snapshot per provider. Descendant activity must clear on a collapsed row
  /// without waiting for another full sidebar catalog read.
  pub(crate) fn session_notifications(&self, keys: &[String]) -> Vec<Value> {
    let mut requested = HashMap::<ViewerProvider, HashSet<SessionLocator>>::new();
    for key in keys {
      if self.check_session_scope(key).is_ok()
        && let Ok(locator) = decode_session_key(key)
      {
        requested.entry(locator.provider).or_default().insert(locator);
      }
    }
    let mut notifications = BTreeMap::new();
    for (provider, locators) in requested {
      let external_catalog = self.relay.external_catalog_covers(provider);
      let (inventory, indexed) = if external_catalog {
        let Ok(Some(inventory)) = self.indexed_session_inventory(provider) else {
          continue;
        };
        (inventory, HashMap::new())
      } else {
        // A committed catalog proves membership and deletion. A cold provider
        // must not emit tombstones or synchronously fall back to its storage.
        let Ok(Some(_)) = self.session_index.source_state(&index_catalog_source_key(provider)) else {
          continue;
        };
        let Ok(rows) = self.session_index.list_all_sessions_for_provider(provider.as_str()) else {
          continue;
        };
        #[cfg(test)]
        {
          let mut stats = self.notification_work_stats.lock().unwrap();
          stats.provider_snapshots += 1;
          stats.rows_loaded += rows.len();
        }
        // Preserve the index's stable order when selecting canonical duplicate
        // identities. The same snapshot also supplies compact messages and
        // tombstones, without a second full row decode or cloning its payloads.
        let mut inventory = SessionHeaderInventory::default();
        for row in rows.iter().filter(|row| row.present) {
          let Ok(header) = indexed_session_header(row) else {
            continue;
          };
          inventory
            .direct_attention
            .insert(locator_for_header(provider, &header), SessionAttention::from_index(row));
          inventory.headers.push(header);
        }
        let indexed = rows.into_iter().map(|row| (row.key.clone(), row)).collect();
        (self.scope_inventory(provider, inventory), indexed)
      };
      let notification = |key: String, attention: SessionAttention| {
        // Each row carries its own compact body projection and source checkpoint.
        // Ancestor attention propagation must never carry the child's message.
        let message = decode_session_key(&key)
          .ok()
          .and_then(|locator| index_session_key(&locator).ok())
          .and_then(|indexed_key| indexed.get(&indexed_key))
          .filter(|row| row.attention_baselined)
          .and_then(|row| {
            let compact = row.latest_message.as_ref()?;
            let value = serde_json::from_str::<Value>(compact).ok()?;
            let revision = format!(
              "index-message.v1.{}.{:x}",
              row.source_generation,
              Sha256::digest(row.source_cursor.as_bytes())
            );
            Some((value, revision))
          });
        json!({
          "session_key": key, "has_unread": attention.has_unread,
          "unread_final_count": attention.unread_final_count, "is_running": attention.is_running,
          "has_running_descendant": attention.has_running_descendant,
          "question_attention": attention.question_attention,
          "latest_assistant_message": message.as_ref().map(|(value, _)| value),
          "message_revision": message.as_ref().map(|(_, revision)| revision),
        })
      };
      // A duplicate source can change while another source owns its canonical
      // identity. Still acknowledge that requested record; only the canonical
      // relation graph is allowed to propagate activity to ancestors.
      for header in &inventory.headers {
        let locator = locator_for_header(provider, header);
        if locators.contains(&locator)
          && let Ok(key) = encode_session_key(&locator)
        {
          let attention = inventory.direct_attention.get(&locator).copied().unwrap_or_default();
          notifications.insert(key.clone(), notification(key, attention));
        }
      }
      let relations = session_relation_index(provider, inventory.headers, &mut Vec::new());
      let attention = session_relation_attention(provider, &relations, &inventory.direct_attention);
      let mut affected = BTreeSet::new();
      for (index, header) in relations.headers.iter().enumerate() {
        if !locators.contains(&locator_for_header(provider, header)) {
          continue;
        }
        let mut next = Some(index);
        while let Some(index) = next {
          if !affected.insert(index) {
            break;
          }
          next = relations.parent_indices[index];
        }
      }
      for index in affected {
        if let Ok(key) = encode_session_key(&locator_for_header(provider, &relations.headers[index])) {
          notifications.insert(key.clone(), notification(key, attention[index]));
        }
      }
      if !external_catalog {
        for locator in &locators {
          let Ok(key) = encode_session_key(locator) else {
            continue;
          };
          if notifications.contains_key(&key) {
            continue;
          }
          let Ok(indexed_key) = index_session_key(locator) else {
            continue;
          };
          // An available committed provider catalog proves deletion. A source
          // read failure or unavailable external catalog does not.
          if indexed.get(&indexed_key).is_none_or(|row| !row.present) {
            notifications.insert(
              key.clone(),
              json!({
                "session_key": key, "removed": true,
                "latest_assistant_message": null, "message_revision": "removed",
                "has_unread": false, "unread_final_count": 0, "is_running": false,
                "has_running_descendant": false,
                "question_attention": {"required_count": 0, "available_count": 0},
              }),
            );
          }
        }
      }
    }
    notifications.into_values().collect()
  }
}
