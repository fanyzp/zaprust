//! Business surfaces' live wiring (parity P10): the apply
//! side of the backend's business, label and quick-reply
//! events, the user actions that send the matching commands,
//! and the archive write-through that makes the synced sets
//! survive a restart.
//!
//! Discipline, per the design standard: label and quick
//! reply saves and toggles apply optimistically with a
//! pending snapshot and roll back on the family's typed
//! failure event; deletions wait for their ack (the
//! clear-chat discipline), because the sync echo of a delete
//! is indistinguishable from another device's. Fetches answer
//! with typed Fetched/Failed pairs and are never optimistic.

use super::*;
use crate::state::{OrderDialog, OrderDialogState, OwnBusinessDayForm, OwnBusinessForm};

impl crate::state::App {
    /// Loads the persisted label and quick-reply sets from
    /// the archive at live start: the launch seed the sync
    /// events then correct.
    pub(super) fn load_business_archive(&mut self, store: &store::Store) {
        if let Ok(labels) = store.labels() {
            self.business.labels = labels;
        }
        if let Ok(rows) = store.chat_labels() {
            for (label_id, chat_jid) in rows {
                self.business.chat_labels.entry(chat_jid).or_default().insert(label_id);
            }
        }
        if let Ok(rows) = store.message_labels() {
            for (label_id, chat_jid, message_id) in rows {
                self.business
                    .message_labels
                    .entry((chat_jid, message_id))
                    .or_default()
                    .insert(label_id);
            }
        }
        if let Ok(replies) = store.quick_replies() {
            self.business.quick_replies = replies;
        }
    }

    fn business_store(&self) -> Option<&store::Store> {
        self.live.as_ref().and_then(|s| s.store.as_ref())
    }

    // -----------------------------------------------------
    // Labels: sync adoption
    // -----------------------------------------------------

    /// Adopts one label definition report: a save, a rename
    /// or recolor from any device, a deletion, or one line of
    /// the pairing-time full sync. Also settles a pending own
    /// save for the same label: the sync state is the truth
    /// either way.
    pub(super) fn apply_label_edited(
        &mut self,
        label_id: String,
        name: Option<String>,
        color: Option<i32>,
        predefined_id: Option<i32>,
        deleted: bool,
    ) {
        self.business.pending_label_saves.remove(&label_id);
        if self.business.label_editor.as_ref().is_some_and(|f| f.id.as_deref() == Some(&label_id)) {
            self.business.label_editor = None;
            self.business.label_editor_error = None;
        }
        if deleted {
            self.remove_label_everywhere(&label_id);
            return;
        }
        let label = backend::LiveLabel {
            id: label_id.clone(),
            name: name.unwrap_or_default(),
            color: color.unwrap_or(0),
            predefined_id,
        };
        match self.business.labels.iter_mut().find(|l| l.id == label_id) {
            Some(slot) => *slot = label.clone(),
            None => self.business.labels.push(label.clone()),
        }
        if let Some(store) = self.business_store() {
            let _ = store.set_label(&label);
        }
    }

    /// Removes a label and every association it had, from the
    /// mirror and the archive. Shared by the sync deletion
    /// report and the delete ack.
    fn remove_label_everywhere(&mut self, label_id: &str) {
        self.business.labels.retain(|l| l.id != label_id);
        for set in self.business.chat_labels.values_mut() {
            set.remove(label_id);
        }
        for set in self.business.message_labels.values_mut() {
            set.remove(label_id);
        }
        if self.business.label_filter.as_deref() == Some(label_id) {
            self.business.label_filter = None;
        }
        if let Some(store) = self.business_store() {
            let _ = store.delete_label(label_id);
        }
    }

    /// Adopts one chat-label association report and settles a
    /// pending own toggle for the same pair.
    pub(super) fn apply_chat_label_changed(
        &mut self,
        label_id: String,
        chat_jid: String,
        labeled: bool,
    ) {
        self.business.pending_chat_labels.remove(&(label_id.clone(), chat_jid.clone()));
        set_association(&mut self.business.chat_labels, chat_jid.clone(), &label_id, labeled);
        if let Some(store) = self.business_store() {
            let _ = store.set_chat_label(&label_id, &chat_jid, labeled);
        }
    }

    /// Adopts one message-label association report and
    /// settles a pending own toggle for the same triple.
    pub(super) fn apply_message_label_changed(
        &mut self,
        label_id: String,
        chat_jid: String,
        message_id: String,
        labeled: bool,
    ) {
        self.business.pending_message_labels.remove(&(
            label_id.clone(),
            chat_jid.clone(),
            message_id.clone(),
        ));
        set_association(
            &mut self.business.message_labels,
            (chat_jid.clone(), message_id.clone()),
            &label_id,
            labeled,
        );
        if let Some(store) = self.business_store() {
            let _ = store.set_message_label(&label_id, &chat_jid, &message_id, labeled);
        }
    }

    /// Rolls one failed label action back to its pending
    /// snapshot and says so: the optimistic apply is undone
    /// exactly, never approximated.
    pub(super) fn apply_label_action_failed(
        &mut self,
        action: backend::LabelAction,
        message: String,
    ) {
        match action {
            backend::LabelAction::Save { label_id, .. } => {
                if let Some(previous) = self.business.pending_label_saves.remove(&label_id) {
                    match previous {
                        Some(label) => {
                            match self.business.labels.iter_mut().find(|l| l.id == label_id) {
                                Some(slot) => *slot = label,
                                None => self.business.labels.push(label),
                            }
                        }
                        None => self.business.labels.retain(|l| l.id != label_id),
                    }
                }
                if self.business.label_editor.is_some() {
                    self.business.label_editor_error = Some(message.clone());
                }
            }
            backend::LabelAction::SetChat { label_id, chat_jid, .. } => {
                if let Some(was) =
                    self.business.pending_chat_labels.remove(&(label_id.clone(), chat_jid.clone()))
                {
                    set_association(&mut self.business.chat_labels, chat_jid, &label_id, was);
                }
            }
            backend::LabelAction::SetMessage { label_id, chat_jid, message_id, .. } => {
                if let Some(was) = self.business.pending_message_labels.remove(&(
                    label_id.clone(),
                    chat_jid.clone(),
                    message_id.clone(),
                )) {
                    set_association(
                        &mut self.business.message_labels,
                        (chat_jid, message_id),
                        &label_id,
                        was,
                    );
                }
            }
        }
        self.notice = Some(crate::state::Notice::error(message));
    }

    /// The delete ack: the label goes everywhere at once.
    pub(super) fn apply_label_deleted(&mut self, label_id: String) {
        self.remove_label_everywhere(&label_id);
        self.notice = Some(crate::state::Notice::info(crate::i18n::t("labels.deleted")));
    }

    /// A delete the server refused: nothing was applied, so
    /// there is nothing to roll back; the failure is spoken.
    pub(super) fn apply_label_delete_failed(&mut self, message: String) {
        self.notice = Some(crate::state::Notice::error(message));
    }

    // -----------------------------------------------------
    // Labels: user actions
    // -----------------------------------------------------

    /// Generates an id for a new label or quick reply:
    /// session-unique without a clock read at the call site.
    fn next_business_id(&mut self, prefix: &str) -> String {
        self.business.id_counter += 1;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        format!("{prefix}-{stamp}-{}", self.business.id_counter)
    }

    /// Submits the label editor: validates, applies at once
    /// with a snapshot, and sends the save. The editor stays
    /// open until the sync echo settles it or the failure
    /// lands inline.
    pub(crate) fn submit_label_editor(&mut self) {
        let (draft_name, draft_id, color) = match &self.business.label_editor {
            Some(form) => (form.name.clone(), form.id.clone(), form.color),
            None => return,
        };
        let name = draft_name.trim().to_owned();
        if name.is_empty() {
            self.business.label_editor_error = Some(crate::i18n::t("labels.name_required"));
            return;
        }
        let (id, is_new) = match draft_id {
            Some(id) => (id, false),
            None => (self.next_business_id("label"), true),
        };
        let label =
            backend::LiveLabel { id: id.clone(), name: name.clone(), color, predefined_id: None };
        let previous = self.business.labels.iter().find(|l| l.id == id).cloned();
        if !is_new {
            if let Some(slot) = self.business.labels.iter_mut().find(|l| l.id == id) {
                slot.name = name.clone();
                slot.color = color;
            }
        } else {
            self.business.labels.push(label);
        }
        self.business.pending_label_saves.insert(id.clone(), previous);
        self.business.label_editor_error = None;
        if let Some(form) = &mut self.business.label_editor {
            form.id = Some(id.clone());
        }
        self.send_backend(backend::BackendCommand::SaveLabel { label_id: id, name, color });
    }

    /// Asks for the delete confirmation for one label.
    pub(crate) fn request_delete_label(&mut self, label_id: &str) {
        let name = self
            .business
            .labels
            .iter()
            .find(|l| l.id == label_id)
            .map_or_else(|| label_id.to_owned(), |l| l.name.clone());
        self.ui_confirm =
            Some(crate::state::UiConfirm::DeleteLabel { label_id: label_id.to_owned(), name });
    }

    /// Sends the confirmed label deletion. Nothing is applied
    /// locally: the ack (or the sync deletion report) removes
    /// the label.
    pub(crate) fn delete_label_confirmed(&mut self, label_id: &str) {
        self.send_backend(backend::BackendCommand::DeleteLabel { label_id: label_id.to_owned() });
    }

    /// Toggles one label on one chat: optimistic, with the
    /// previous state snapshotted for the failure rollback.
    pub(crate) fn set_chat_label(&mut self, label_id: &str, chat_id: u64, labeled: bool) {
        let Some(chat_jid) = self.live_jid(chat_id) else { return };
        let was =
            self.business.chat_labels.get(&chat_jid).is_some_and(|set| set.contains(label_id));
        if was == labeled {
            return;
        }
        self.business
            .pending_chat_labels
            .entry((label_id.to_owned(), chat_jid.clone()))
            .or_insert(was);
        set_association(&mut self.business.chat_labels, chat_jid.clone(), label_id, labeled);
        self.send_backend(backend::BackendCommand::SetChatLabel {
            label_id: label_id.to_owned(),
            chat_jid,
            labeled,
        });
    }

    /// Toggles one label on one message: optimistic, like the
    /// chat toggle. The message is addressed by its protocol
    /// id, resolved from the session's binding.
    pub(crate) fn set_message_label(
        &mut self,
        label_id: &str,
        chat_id: u64,
        msg_id: u64,
        labeled: bool,
    ) {
        let (Some(chat_jid), Some(proto_id)) =
            (self.live_jid(chat_id), self.protocol_ids.get(&msg_id).cloned())
        else {
            return;
        };
        let key = (chat_jid.clone(), proto_id.clone());
        let was = self.business.message_labels.get(&key).is_some_and(|set| set.contains(label_id));
        if was == labeled {
            return;
        }
        self.business
            .pending_message_labels
            .entry((label_id.to_owned(), chat_jid.clone(), proto_id.clone()))
            .or_insert(was);
        set_association(&mut self.business.message_labels, key, label_id, labeled);
        self.send_backend(backend::BackendCommand::SetMessageLabel {
            label_id: label_id.to_owned(),
            chat_jid,
            message_id: proto_id,
            labeled,
        });
    }

    // -----------------------------------------------------
    // Quick replies: sync adoption and actions
    // -----------------------------------------------------

    /// Adopts one quick reply report (a save from any device,
    /// a deletion, or one line of the full sync) and settles
    /// a pending own save for the same id.
    pub(super) fn apply_quick_reply_edited(
        &mut self,
        reply: backend::LiveQuickReply,
        deleted: bool,
    ) {
        self.business.pending_quick_saves.remove(&reply.id);
        if self.business.quick_editor.as_ref().is_some_and(|f| f.id.as_deref() == Some(&reply.id)) {
            self.business.quick_editor = None;
            self.business.quick_editor_error = None;
        }
        if deleted {
            self.business.quick_replies.retain(|r| r.id != reply.id);
            if let Some(store) = self.business_store() {
                let _ = store.delete_quick_reply(&reply.id);
            }
            return;
        }
        match self.business.quick_replies.iter_mut().find(|r| r.id == reply.id) {
            Some(slot) => *slot = reply.clone(),
            None => self.business.quick_replies.push(reply.clone()),
        }
        if let Some(store) = self.business_store() {
            let _ = store.set_quick_reply(&reply);
        }
    }

    /// Rolls one failed quick reply save back to its snapshot.
    pub(super) fn apply_quick_reply_failed(&mut self, id: String, message: String) {
        if let Some(previous) = self.business.pending_quick_saves.remove(&id) {
            match previous {
                Some(reply) => match self.business.quick_replies.iter_mut().find(|r| r.id == id) {
                    Some(slot) => *slot = reply,
                    None => self.business.quick_replies.push(reply),
                },
                None => self.business.quick_replies.retain(|r| r.id != id),
            }
        }
        if self.business.quick_editor.is_some() {
            self.business.quick_editor_error = Some(message);
        } else {
            self.notice = Some(crate::state::Notice::error(message));
        }
    }

    /// The quick reply delete ack.
    pub(super) fn apply_quick_reply_deleted(&mut self, id: String) {
        self.business.quick_replies.retain(|r| r.id != id);
        if let Some(store) = self.business_store() {
            let _ = store.delete_quick_reply(&id);
        }
        self.notice = Some(crate::state::Notice::info(crate::i18n::t("business.quick_deleted")));
    }

    /// A quick reply delete the server refused.
    pub(super) fn apply_quick_reply_delete_failed(&mut self, message: String) {
        self.notice = Some(crate::state::Notice::error(message));
    }

    /// Submits the quick reply editor: validates, applies at
    /// once with a snapshot, and sends the save.
    pub(crate) fn submit_quick_editor(&mut self) {
        let (draft_shortcut, draft_message, draft_keywords, draft_id) =
            match &self.business.quick_editor {
                Some(form) => (
                    form.shortcut.clone(),
                    form.message.clone(),
                    form.keywords.clone(),
                    form.id.clone(),
                ),
                None => return,
            };
        let shortcut = draft_shortcut.trim().trim_start_matches('/').to_owned();
        let message = draft_message.trim().to_owned();
        if shortcut.is_empty() || message.is_empty() {
            self.business.quick_editor_error = Some(crate::i18n::t("business.quick_required"));
            return;
        }
        if self
            .business
            .quick_replies
            .iter()
            .any(|r| r.shortcut.eq_ignore_ascii_case(&shortcut) && Some(&r.id) != draft_id.as_ref())
        {
            self.business.quick_editor_error = Some(crate::i18n::t("business.quick_duplicate"));
            return;
        }
        let keywords: Vec<String> = draft_keywords
            .split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_owned)
            .collect();
        let (id, count) = match &draft_id {
            Some(id) => (
                id.clone(),
                self.business.quick_replies.iter().find(|r| &r.id == id).map_or(0, |r| r.count),
            ),
            None => (self.next_business_id("qr"), 0),
        };
        let reply = backend::LiveQuickReply {
            id: id.clone(),
            shortcut: shortcut.clone(),
            message: message.clone(),
            keywords: keywords.clone(),
            count,
        };
        let previous = self.business.quick_replies.iter().find(|r| r.id == id).cloned();
        match self.business.quick_replies.iter_mut().find(|r| r.id == id) {
            Some(slot) => *slot = reply,
            None => self.business.quick_replies.push(reply),
        }
        self.business.pending_quick_saves.insert(id.clone(), previous);
        self.business.quick_editor_error = None;
        if let Some(form) = &mut self.business.quick_editor {
            form.id = Some(id.clone());
        }
        self.send_backend(backend::BackendCommand::SaveQuickReply {
            id,
            shortcut,
            message,
            keywords,
            count,
        });
    }

    /// Asks for the delete confirmation for one quick reply.
    pub(crate) fn request_delete_quick_reply(&mut self, id: &str) {
        let shortcut = self
            .business
            .quick_replies
            .iter()
            .find(|r| r.id == id)
            .map_or_else(|| id.to_owned(), |r| r.shortcut.clone());
        self.ui_confirm =
            Some(crate::state::UiConfirm::DeleteQuickReply { id: id.to_owned(), shortcut });
    }

    /// Sends the confirmed quick reply deletion (ack-gated,
    /// like the label deletion).
    pub(crate) fn delete_quick_reply_confirmed(&mut self, id: &str) {
        self.send_backend(backend::BackendCommand::DeleteQuickReply { id: id.to_owned() });
    }

    // -----------------------------------------------------
    // Business profiles, catalogs, orders: fetches
    // -----------------------------------------------------

    /// Asks for one chat's business profile unless it is
    /// already here or on its way. Called when a DM's profile
    /// dock opens.
    pub(crate) fn ensure_business_profile(&mut self, chat_jid: &str) {
        if self.live.is_none()
            || self.business.profiles.contains_key(chat_jid)
            || self.business.profile_fetching.contains(chat_jid)
        {
            return;
        }
        self.business.profile_fetching.insert(chat_jid.to_owned());
        self.send_backend(backend::BackendCommand::FetchBusinessProfile {
            chat_jid: chat_jid.to_owned(),
        });
    }

    /// Adopts one business profile answer.
    pub(super) fn apply_business_profile_fetched(
        &mut self,
        chat_jid: String,
        profile: Option<backend::LiveBusinessProfile>,
    ) {
        self.business.profile_fetching.remove(&chat_jid);
        self.business.profiles_failed.remove(&chat_jid);
        self.business.profiles.insert(chat_jid, profile);
    }

    /// Records one business profile fetch failure.
    pub(super) fn apply_business_profile_fetch_failed(
        &mut self,
        chat_jid: String,
        message: String,
    ) {
        self.business.profile_fetching.remove(&chat_jid);
        self.business.profiles_failed.insert(chat_jid, message);
    }

    /// Opens the dock's catalog view for one business and
    /// starts its first page and collections fetches.
    /// Adopts one catalog page: products append, hidden ones
    /// are dropped at the door (they are merchant-side data,
    /// not customer-facing).
    pub(super) fn apply_catalog_fetched(
        &mut self,
        chat_jid: String,
        products: Vec<backend::LiveProduct>,
        after_cursor: Option<String>,
    ) {
        let state =
            self.business.catalogs.entry(chat_jid).or_insert_with(|| crate::state::CatalogState {
                products: Vec::new(),
                after: None,
                loading: false,
                failed: None,
                collections: None,
                collections_failed: None,
            });
        state.loading = false;
        state.failed = None;
        for product in products {
            if !product.hidden {
                state.products.push(product);
            }
        }
        state.after = after_cursor;
    }

    /// Records one catalog page failure.
    pub(super) fn apply_catalog_fetch_failed(&mut self, chat_jid: String, message: String) {
        if let Some(state) = self.business.catalogs.get_mut(&chat_jid) {
            state.loading = false;
            state.failed = Some(message);
        }
    }

    /// Adopts one collections answer.
    pub(super) fn apply_collections_fetched(
        &mut self,
        chat_jid: String,
        collections: Vec<backend::LiveCollection>,
    ) {
        let state =
            self.business.catalogs.entry(chat_jid).or_insert_with(|| crate::state::CatalogState {
                products: Vec::new(),
                after: None,
                loading: false,
                failed: None,
                collections: None,
                collections_failed: None,
            });
        state.collections = Some(collections);
        state.collections_failed = None;
    }

    /// Records one collections failure.
    pub(super) fn apply_collections_fetch_failed(&mut self, chat_jid: String, message: String) {
        if let Some(state) = self.business.catalogs.get_mut(&chat_jid) {
            state.collections_failed = Some(message);
        }
    }

    /// Ensures one product image is on its way, called by the
    /// catalog tiles that paint it.
    /// Adopts one product image's bytes: decoded here, where
    /// a graphics context exists, like avatar bytes.
    pub(super) fn apply_product_image_bytes(
        &mut self,
        ctx: &egui::Context,
        url: String,
        bytes: Vec<u8>,
    ) {
        let state = match crate::media::decode_photo(ctx, &bytes) {
            Some(tex) => crate::state::ProductImageState::Ready(tex),
            None => crate::state::ProductImageState::Failed,
        };
        self.business.product_images.insert(url, state);
    }

    /// Records one product image failure.
    pub(super) fn apply_product_image_fetch_failed(&mut self, url: String) {
        self.business.product_images.insert(url, crate::state::ProductImageState::Failed);
    }

    /// Opens the order details dialog for one order message
    /// and starts the details fetch. A message whose payload
    /// carried no token cannot authorize a fetch: the dialog
    /// opens on the summary alone and says why there is no
    /// more to load.
    pub(crate) fn open_order_dialog(&mut self, chat_id: u64, msg_id: u64) {
        let Some(chat) = self.conversation.iter().find(|m| m.id == msg_id) else { return };
        let core::model::MessageKind::Order {
            order_id,
            token,
            item_count,
            total_amount_1000,
            currency,
            status,
        } = &chat.kind
        else {
            return;
        };
        let (order_id, token) = (order_id.clone(), token.clone());
        let (item_count, total_amount_1000, currency, status) =
            (*item_count, *total_amount_1000, currency.clone(), status.clone());
        let Some(chat_jid) = self.live_jid(chat_id) else { return };
        let state = if token.is_empty() {
            OrderDialogState::Failed(crate::i18n::t("business.order_no_token"))
        } else {
            OrderDialogState::Loading
        };
        self.business.order_dialog = Some(OrderDialog {
            chat_jid: chat_jid.clone(),
            order_id: order_id.clone(),
            item_count,
            total_amount_1000,
            currency,
            status,
            state,
        });
        if token.is_empty() {
            return;
        }
        self.send_backend(backend::BackendCommand::FetchOrderDetails { chat_jid, order_id, token });
    }

    /// Adopts one order details answer, if the dialog it
    /// belongs to is still open on that order.
    pub(super) fn apply_order_details_fetched(
        &mut self,
        chat_jid: String,
        order_id: String,
        order: backend::LiveOrder,
    ) {
        if let Some(dialog) = &mut self.business.order_dialog {
            if dialog.chat_jid == chat_jid && dialog.order_id == order_id {
                dialog.state = OrderDialogState::Loaded(order);
            }
        }
    }

    /// Records one order details failure, same matching.
    pub(super) fn apply_order_details_fetch_failed(
        &mut self,
        chat_jid: String,
        order_id: String,
        message: String,
    ) {
        if let Some(dialog) = &mut self.business.order_dialog {
            if dialog.chat_jid == chat_jid && dialog.order_id == order_id {
                dialog.state = OrderDialogState::Failed(message);
            }
        }
    }

    // -----------------------------------------------------
    // Own business profile
    // -----------------------------------------------------

    /// Asks for the account's own business profile unless it
    /// is already here. Called when Settings > Business opens.
    pub(crate) fn ensure_own_business_profile(&mut self) {
        if self.live.is_none() || self.business.own_profile.is_some() {
            return;
        }
        self.send_backend(backend::BackendCommand::FetchOwnBusinessProfile);
    }

    /// Adopts the own business profile answer. An open editor
    /// is rebuilt from the fresh profile: the server's state
    /// is what the form edits.
    pub(super) fn apply_own_business_fetched(
        &mut self,
        profile: Option<backend::LiveBusinessProfile>,
    ) {
        self.business.own_profile_failed = None;
        self.business.own_profile = Some(profile);
        if self.business.own_form.is_some() {
            self.business.own_form =
                self.business.own_profile.as_ref().and_then(|p| p.as_ref()).map(own_business_form);
        }
    }

    /// Records the own profile fetch failure.
    pub(super) fn apply_own_business_fetch_failed(&mut self, message: String) {
        self.business.own_profile_failed = Some(message);
    }

    /// The own profile save landed: the editor closes and the
    /// profile is re-read, so the page shows what the server
    /// kept.
    pub(super) fn apply_own_business_saved(&mut self) {
        self.business.own_saving = false;
        self.business.own_form = None;
        self.business.own_error = None;
        self.business.own_profile = None;
        self.notice = Some(crate::state::Notice::info(crate::i18n::t("business.profile_saved")));
        self.ensure_own_business_profile();
    }

    /// The own profile save failed: the editor stays, with
    /// the failure inline.
    pub(super) fn apply_own_business_save_failed(&mut self, message: String) {
        self.business.own_saving = false;
        self.business.own_error = Some(message);
    }

    /// Opens the own profile editor from the fetched profile.
    pub(crate) fn open_own_business_form(&mut self) {
        if let Some(profile) = self.business.own_profile.as_ref().and_then(|p| p.as_ref()) {
            self.business.own_form = Some(own_business_form(profile));
            self.business.own_error = None;
        }
    }

    /// Submits the own profile editor: builds the delta
    /// against the fetched profile and sends only what
    /// changed. An unchanged form just closes.
    pub(crate) fn submit_own_business_form(&mut self) {
        let (Some(form), Some(profile)) = (
            self.business.own_form.clone(),
            self.business.own_profile.as_ref().and_then(|p| p.clone()),
        ) else {
            return;
        };
        let Some(update) = own_business_delta(&form, &profile) else {
            self.business.own_form = None;
            return;
        };
        self.business.own_saving = true;
        self.business.own_error = None;
        self.send_backend(backend::BackendCommand::UpdateOwnBusinessProfile { update });
    }
}

/// Sets or clears one association in a JID-keyed map,
/// dropping empty sets so the maps only carry live pairs.
fn set_association<K: std::hash::Hash + Eq>(
    map: &mut std::collections::HashMap<K, std::collections::HashSet<String>>,
    key: K,
    label_id: &str,
    labeled: bool,
) {
    if labeled {
        map.entry(key).or_default().insert(label_id.to_owned());
    } else if let Some(set) = map.get_mut(&key) {
        set.remove(label_id);
    }
}

/// The composer's quick-reply matches for a draft: while
/// the draft is a single slash token, the synced replies
/// whose shortcut starts with the token or whose keywords
/// contain it, most-used first, capped at six. Pure, so the
/// popup's behavior is testable without a frame.
pub(crate) fn quick_reply_matches(
    replies: &[backend::LiveQuickReply],
    draft: &str,
) -> Vec<backend::LiveQuickReply> {
    let Some(token) = draft.strip_prefix('/') else {
        return Vec::new();
    };
    if token.contains(char::is_whitespace) {
        return Vec::new();
    }
    let token = token.to_lowercase();
    let mut hits: Vec<backend::LiveQuickReply> = replies
        .iter()
        .filter(|r| {
            r.shortcut.to_lowercase().starts_with(&token)
                || r.keywords.iter().any(|k| k.to_lowercase().starts_with(&token))
        })
        .cloned()
        .collect();
    hits.sort_by_key(|r| std::cmp::Reverse(r.count));
    hits.truncate(6);
    hits
}

/// The label palette: the wire's color index mapped through
/// core's fixed table (`core::theme::LABEL_PALETTE`, kept in
/// core so its contrast tests can pin every color). Indices
/// outside the table wrap, so a color a newer client chose
/// still paints deterministically.
pub(crate) fn label_color(index: i32) -> core::theme::Rgb {
    let wrapped = usize::try_from(index.rem_euclid(20)).unwrap_or(0);
    core::theme::LABEL_PALETTE[wrapped]
}

/// Formats an amount in thousandths of a currency unit:
/// the currency code, the grouped whole part, and the
/// fraction only when there is one.
pub(crate) fn format_money(amount_1000: i64, currency: &str) -> String {
    let sign = if amount_1000 < 0 { "-" } else { "" };
    let abs = amount_1000.unsigned_abs();
    let whole = abs / 1000;
    let frac = abs % 1000;
    let sep = if crate::i18n::locale() == "id" { '.' } else { ',' };
    let digits = whole.to_string();
    let mut grouped = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(sep);
        }
        grouped.push(ch);
    }
    let dec_sep = if crate::i18n::locale() == "id" { ',' } else { '.' };
    let amount = if frac == 0 {
        grouped
    } else if frac % 10 == 0 {
        format!("{grouped}{dec_sep}{:02}", frac / 10)
    } else {
        format!("{grouped}{dec_sep}{frac:03}")
    };
    if currency.is_empty() {
        format!("{sign}{amount}")
    } else {
        format!("{currency} {sign}{amount}")
    }
}

/// The seven form days, Monday first, by wire spelling.
const FORM_DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Formats minutes past midnight as `HH:MM` for the form.
fn minutes_text(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Parses an `HH:MM` field back to minutes past midnight.
fn parse_minutes(text: &str) -> Option<u32> {
    let (h, m) = text.trim().split_once(':')?;
    let hours: u32 = h.parse().ok()?;
    let minutes: u32 = m.parse().ok()?;
    if hours > 24 || minutes > 59 || (hours == 24 && minutes > 0) {
        return None;
    }
    Some(hours * 60 + minutes)
}

/// Builds the editor draft from a fetched profile.
pub(crate) fn own_business_form(profile: &backend::LiveBusinessProfile) -> OwnBusinessForm {
    OwnBusinessForm {
        description: profile.description.clone(),
        email: profile.email.clone().unwrap_or_default(),
        address: profile.address.clone().unwrap_or_default(),
        websites: profile.websites.join("\n"),
        kept_categories: profile.categories.iter().map(|c| c.id.clone()).collect(),
        timezone: profile.hours_timezone.clone().unwrap_or_default(),
        days: FORM_DAYS
            .iter()
            .map(|day| {
                let stated = profile.hours.iter().find(|h| h.day == *day);
                OwnBusinessDayForm {
                    day,
                    mode: stated.map_or_else(|| "closed".to_owned(), |h| h.mode.clone()),
                    open: stated
                        .and_then(|h| h.open_minutes)
                        .map_or_else(|| "08:00".to_owned(), minutes_text),
                    close: stated
                        .and_then(|h| h.close_minutes)
                        .map_or_else(|| "17:00".to_owned(), minutes_text),
                }
            })
            .collect(),
    }
}

/// Builds the save delta from the draft against the fetched
/// profile: `None` when nothing changed. Text fields map
/// the form's empty string to a clear, exactly the
/// contract's semantics.
pub(crate) fn own_business_delta(
    form: &OwnBusinessForm,
    profile: &backend::LiveBusinessProfile,
) -> Option<backend::LiveBusinessProfileUpdate> {
    let mut update = backend::LiveBusinessProfileUpdate::default();
    let text_delta = |typed: &str, current: Option<&str>| -> Option<Option<String>> {
        let typed = typed.trim();
        if typed == current.unwrap_or("") {
            None
        } else if typed.is_empty() {
            Some(None)
        } else {
            Some(Some(typed.to_owned()))
        }
    };
    if form.description.trim() != profile.description {
        update.description = Some(if form.description.trim().is_empty() {
            None
        } else {
            Some(form.description.trim().to_owned())
        });
    }
    update.email = text_delta(&form.email, profile.email.as_deref());
    update.address = text_delta(&form.address, profile.address.as_deref());
    let websites: Vec<String> = form
        .websites
        .split(['\n', ','])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    if websites != profile.websites {
        update.websites = Some(websites);
    }
    let current_categories: Vec<String> = profile.categories.iter().map(|c| c.id.clone()).collect();
    if form.kept_categories != current_categories {
        update.categories = Some(form.kept_categories.clone());
    }
    let days: Vec<backend::LiveBusinessDayHours> = form
        .days
        .iter()
        .filter(|d| d.mode != "closed")
        .map(|d| backend::LiveBusinessDayHours {
            day: d.day.to_owned(),
            mode: d.mode.clone(),
            open_minutes: if d.mode == "specific_hours" { parse_minutes(&d.open) } else { None },
            close_minutes: if d.mode == "specific_hours" { parse_minutes(&d.close) } else { None },
        })
        .collect();
    let timezone =
        if form.timezone.trim().is_empty() { None } else { Some(form.timezone.trim().to_owned()) };
    if days != profile.hours || timezone != profile.hours_timezone {
        update.hours = Some(backend::LiveBusinessHoursUpdate { timezone, note: None, days });
    }
    if update == backend::LiveBusinessProfileUpdate::default() {
        None
    } else {
        Some(update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare app for the state-machine tests: no live
    /// session, so sends no-op and the store write-through is
    /// skipped; the optimistic bookkeeping is what is under
    /// test.
    fn bare_app() -> crate::state::App {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        crate::state::App::new(crate::state::Args::parse(), &ctx)
    }

    fn a_label(id: &str, name: &str) -> backend::LiveLabel {
        backend::LiveLabel { id: id.into(), name: name.into(), color: 3, predefined_id: None }
    }

    #[test]
    fn label_save_failure_restores_the_previous_label() {
        let mut app = bare_app();
        app.business.labels.push(a_label("l1", "Old name"));
        // An edit in flight: the optimistic value is in the
        // list, the previous one is the pending snapshot.
        app.business.labels[0].name = "New name".into();
        app.business.pending_label_saves.insert("l1".into(), Some(a_label("l1", "Old name")));
        app.apply_label_action_failed(
            backend::LabelAction::Save { label_id: "l1".into(), name: "New name".into(), color: 3 },
            "nope".into(),
        );
        assert_eq!(app.business.labels[0].name, "Old name");
        assert!(app.business.pending_label_saves.is_empty());
        assert!(app.notice.is_some());
    }

    #[test]
    fn label_create_failure_removes_the_optimistic_label() {
        let mut app = bare_app();
        app.business.labels.push(a_label("l2", "Fresh"));
        app.business.pending_label_saves.insert("l2".into(), None);
        app.apply_label_action_failed(
            backend::LabelAction::Save { label_id: "l2".into(), name: "Fresh".into(), color: 3 },
            "nope".into(),
        );
        assert!(app.business.labels.is_empty());
    }

    #[test]
    fn chat_label_failure_restores_membership() {
        let mut app = bare_app();
        app.business
            .chat_labels
            .entry("123@s.whatsapp.net".into())
            .or_default()
            .insert("l1".into());
        app.business.pending_chat_labels.insert(("l1".into(), "123@s.whatsapp.net".into()), false);
        app.apply_label_action_failed(
            backend::LabelAction::SetChat {
                label_id: "l1".into(),
                chat_jid: "123@s.whatsapp.net".into(),
                labeled: true,
            },
            "nope".into(),
        );
        assert!(!app
            .business
            .chat_labels
            .get("123@s.whatsapp.net")
            .is_some_and(|set| set.contains("l1")));
    }

    #[test]
    fn label_echo_settles_the_pending_save() {
        let mut app = bare_app();
        app.business.pending_label_saves.insert("l1".into(), None);
        app.apply_label_edited("l1".into(), Some("Urgent".into()), Some(5), None, false);
        assert_eq!(app.business.labels.len(), 1);
        assert_eq!(app.business.labels[0].color, 5);
        assert!(app.business.pending_label_saves.is_empty());
    }

    #[test]
    fn quick_reply_failure_keeps_the_editor_open() {
        let mut app = bare_app();
        app.business.quick_editor = Some(crate::state::QuickReplyForm {
            id: None,
            shortcut: "hi".into(),
            message: "Hello".into(),
            keywords: String::new(),
        });
        app.apply_quick_reply_failed("qr-x".into(), "nope".into());
        assert!(app.business.quick_editor.is_some());
        assert_eq!(app.business.quick_editor_error.as_deref(), Some("nope"));
    }

    #[test]
    fn own_business_delta_names_only_what_changed() {
        let profile = backend::LiveBusinessProfile {
            description: "Coffee and beans".into(),
            email: Some("hi@example.com".into()),
            websites: vec!["https://example.com".into()],
            categories: vec![
                backend::LiveBusinessCategory { id: "c1".into(), name: "Cafe".into() },
                backend::LiveBusinessCategory { id: "c2".into(), name: "Bakery".into() },
            ],
            address: Some("1 Main St".into()),
            hours_timezone: Some("Asia/Makassar".into()),
            hours: Vec::new(),
        };
        // An untouched form produces no delta at all.
        let form = own_business_form(&profile);
        assert!(own_business_delta(&form, &profile).is_none());
        // A retyped description and a dropped category are
        // the delta; everything else stays out of it.
        let mut form = own_business_form(&profile);
        form.description = "Coffee, beans, and cake".into();
        form.kept_categories.retain(|id| id != "c2");
        let delta = own_business_delta(&form, &profile).expect("a delta");
        assert_eq!(delta.description, Some(Some("Coffee, beans, and cake".into())));
        assert_eq!(delta.categories, Some(vec!["c1".to_string()]));
        assert_eq!(delta.email, None);
        assert_eq!(delta.websites, None);
        // Clearing a text field is a clear, not an omission.
        let mut form = own_business_form(&profile);
        form.email = String::new();
        let delta = own_business_delta(&form, &profile).expect("a delta");
        assert_eq!(delta.email, Some(None));
    }

    #[test]
    fn money_formats_grouped_with_optional_fraction() {
        crate::i18n::set_locale("en");
        assert_eq!(format_money(205_000_000, "IDR"), "IDR 205,000");
        assert_eq!(format_money(1_990, "USD"), "USD 1.99");
        assert_eq!(format_money(1_995, "USD"), "USD 1.995");
        assert_eq!(format_money(0, ""), "0");
    }

    #[test]
    fn quick_reply_matches_by_shortcut_prefix_and_keyword() {
        let replies = vec![
            backend::LiveQuickReply {
                id: "1".into(),
                shortcut: "hours".into(),
                message: "We open 8 to 21.".into(),
                keywords: vec!["open".into()],
                count: 4,
            },
            backend::LiveQuickReply {
                id: "2".into(),
                shortcut: "ship".into(),
                message: "Ships today.".into(),
                keywords: vec!["shipping".into(), "delivery".into()],
                count: 9,
            },
        ];
        assert_eq!(quick_reply_matches(&replies, "hours").len(), 0);
        assert_eq!(quick_reply_matches(&replies, "/h").len(), 1);
        assert_eq!(quick_reply_matches(&replies, "/deliv").len(), 1);
        assert_eq!(quick_reply_matches(&replies, "/").len(), 2);
        // Most-used first on the empty token.
        assert_eq!(quick_reply_matches(&replies, "/")[0].shortcut, "ship");
        // A space ends the token: the popup steps aside.
        assert!(quick_reply_matches(&replies, "/h there").is_empty());
    }

    #[test]
    fn palette_wraps_out_of_range_indices() {
        assert_eq!(label_color(0), label_color(20));
        assert_eq!(label_color(-1), label_color(19));
        assert_ne!(label_color(1), label_color(2));
    }
}
