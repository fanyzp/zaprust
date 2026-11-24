//! P2-67 stream profile harness: measures the full-render
//! cost of the conversation stream at several conversation
//! sizes, so the windowing decision rests on numbers rather
//! than impressions (qc1617 milestone log, blocker 5). This
//! is a measurement tool, not a gate: it is `#[ignore]`d and
//! run manually, in release profile, on the build box:
//!
//! ```sh
//! cargo test -p app --release stream_profile -- --ignored --nocapture
//! ```
//!
//! It draws the real stream (the same `draw_stream` the app
//! runs) over synthetic text conversations of 500, 2000 and
//! 5000 messages on a headless egui context, and reports the
//! first frame (cold galleys) and the steady-state frame
//! cost (warm galley cache, the number a user lives with).

#[cfg(test)]
mod tests {
    use crate::state::{App, Args};
    use core::model::Message;

    /// A synthetic conversation of `n` text messages spread
    /// over 30 days, alternating senders, with body lengths
    /// cycling through short, medium and long so wrapping
    /// work resembles a real chat.
    fn synthetic_conversation(n: usize) -> Vec<Message> {
        let bodies = [
            "ok",
            "sounds good, see you there",
            "the build finished and the gates are green, shipping it now",
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod \
             tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim \
             veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea \
             commodo consequat. Duis aute irure in reprehenderit.",
        ];
        (0..n)
            .map(|i| {
                let body = bodies[i % bodies.len()];
                let day = format!("{} November 2026", 1 + (i * 30 / n.max(1)));
                let mut m = if i % 2 == 0 {
                    Message::mine(i as u64 + 1, body, "10:00")
                } else {
                    Message::theirs(i as u64 + 1, "Ana", body, "10:00")
                };
                m.day = day;
                m
            })
            .collect()
    }

    #[test]
    #[ignore = "profiling harness: run manually with --ignored --nocapture"]
    fn stream_profile() {
        for n in [500_usize, 2000, 5000] {
            let ctx = egui::Context::default();
            let mut args = Args::parse();
            args.live = false;
            let mut app = App::new(args, &ctx);
            let chat = app.chats.first().expect("demo seeds chats").clone();
            app.selected_chat = chat.id;
            app.conversation = synthetic_conversation(n);
            let tokens = app.tokens();
            let input = |time: f64| egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::Vec2::new(1280.0, 800.0),
                )),
                time: Some(time),
                ..Default::default()
            };
            let draw = |app: &mut App, ctx: &egui::Context, time: f64| {
                let chat = chat.clone();
                ctx.run(input(time), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let mut actions = Vec::new();
                        let mut engaged = false;
                        app.draw_stream(ui, &chat, tokens, time, 120.0, &mut actions, &mut engaged);
                    });
                });
            };
            // Cold frame, then warmup, then the measured run.
            let t0 = std::time::Instant::now();
            draw(&mut app, &ctx, 0.0);
            let cold = t0.elapsed();
            for i in 0..5 {
                draw(&mut app, &ctx, 0.1 + i as f64 * 0.1);
            }
            let frames = 30;
            let t1 = std::time::Instant::now();
            for i in 0..frames {
                draw(&mut app, &ctx, 1.0 + i as f64 * 0.05);
            }
            let steady = t1.elapsed() / frames;
            println!(
                "stream_profile n={n} cold_frame={cold:?} steady_frame={steady:?} \
                 steady_fps={:.0}",
                1.0 / steady.as_secs_f64()
            );
        }
    }
}
