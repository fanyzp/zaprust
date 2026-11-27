//! Roster frame profile harness (qc1644): measures the
//! per-frame roster preparation the chat list pays before it
//! can paint a single row: the visible-chats ordering
//! (`App::visible_chats`), the one-pass roster counts
//! (`chat_list::roster_counts`), and the verified-badge ask
//! (the generation key the badge memo compares). This is a
//! measurement tool, not a gate: it is `#[ignore]`d and run
//! manually, in release profile, on the build box:
//!
//! ```sh
//! cargo test -p app --release roster_frame_profile -- --ignored --nocapture
//! ```
//!
//! It stages 1,200 synthetic chats on a demo-mode app (the
//! roster size a heavy account reaches), warms the memos,
//! then reports the wall time of each phase per frame in
//! steady state, plus the whole preparation end to end. The
//! steady-state numbers are the ones that matter: with the
//! qc1644 memos warm, visible_chats is a key comparison and
//! a borrowed slice, so its phase should sit near zero next
//! to the counts pass, which still walks the roster once.

#[cfg(test)]
mod tests {
    use crate::state::{App, Args};

    /// 1,200 synthetic chats cloned from one demo template,
    /// with distinct IDs, titles and previews, a spread of
    /// kinds and states, so the filter and the counts walk
    /// realistic text.
    fn synthetic_roster(n: usize) -> Vec<core::model::Chat> {
        let template = core::model::demo_chats().into_iter().next().expect("demo seeds chats");
        (0..n)
            .map(|i| {
                let mut chat = template.clone();
                chat.id = 10_000 + i as u64;
                chat.title = format!("Contact number {i} with a reasonably long name");
                chat.preview = format!("preview text for chat {i}, some longer than others");
                chat.unread = (i % 7 == 0) as u32;
                chat.pinned = i % 50 == 0;
                chat.archived = i % 30 == 0;
                chat.favorite = i % 40 == 0;
                chat
            })
            .collect()
    }

    #[test]
    #[ignore = "profiling harness: run manually with --ignored --nocapture"]
    fn roster_frame_profile() {
        let ctx = egui::Context::default();
        let mut app = App::new(Args::for_test(), &ctx);
        // The profile measures roster preparation, not a live
        // session: demo mode, synthetic roster.
        app.live = None;
        app.live_mode = false;
        app.chats = synthetic_roster(1_200);

        // Warmup: the memos fill and the allocator settles.
        for _ in 0..5 {
            let _ = app.visible_chats().len();
            let _ = crate::chat_list::roster_counts(&app.chats);
        }

        let frames = 60;
        let mut visible_t = std::time::Duration::ZERO;
        let mut counts_t = std::time::Duration::ZERO;
        let mut badge_t = std::time::Duration::ZERO;
        for _ in 0..frames {
            let t0 = std::time::Instant::now();
            let visible_len = app.visible_chats().len();
            visible_t += t0.elapsed();
            assert!(visible_len > 0, "the roster yields visible chats");
            std::hint::black_box(visible_len);

            let t1 = std::time::Instant::now();
            let counts = crate::chat_list::roster_counts(&app.chats);
            counts_t += t1.elapsed();
            std::hint::black_box(counts);

            // The badge ask: the key the verified-badge memo
            // compares before it decides to rebuild, read the
            // same way the chat list reads it.
            let t2 = std::time::Instant::now();
            let badge_key =
                app.live.as_ref().map(|s| (s.roster_generation, s.verified_names.len()));
            badge_t += t2.elapsed();
            std::hint::black_box(badge_key);
        }
        let per = |d: std::time::Duration| d / frames;
        println!(
            "roster_frame_profile chats=1200 visible_chats={:?} roster_counts={:?} \
             badge_ask={:?} total_prep={:?} per frame",
            per(visible_t),
            per(counts_t),
            per(badge_t),
            per(visible_t + counts_t + badge_t),
        );
    }
}
