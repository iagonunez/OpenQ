//! System displays (monitors) that a stage can be sent to.
//!
//! winit only lists monitors from inside a running event loop and eframe doesn't share its own,
//! so the list comes from a short-lived child process (`openq --list-displays`). Its order is
//! winit's `available_monitors()` order, which is what egui's `with_monitor(index)` expects.

use std::process::Command;

#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub name: String,
    pub size: [u32; 2],
    pub position: [i32; 2],
    pub refresh_hz: Option<f32>,
}

impl Display {
    pub fn label(&self) -> String {
        let hz = self.refresh_hz.map(|r| format!(" @ {r:.0} Hz")).unwrap_or_default();
        format!("{} — {}×{}{hz}", self.name, self.size[0], self.size[1])
    }
}

pub const LIST_FLAG: &str = "--list-displays";

/// Ask a child process for the current displays.
pub fn list() -> anyhow::Result<Vec<Display>> {
    let out = Command::new(std::env::current_exe()?).arg(LIST_FLAG).output()?;
    anyhow::ensure!(out.status.success(), "display query failed: {}", String::from_utf8_lossy(&out.stderr));
    Ok(parse(&String::from_utf8_lossy(&out.stdout)))
}

fn parse(text: &str) -> Vec<Display> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let [name, w, h, x, y, hz] = f[..] else { return None };
            Some(Display {
                name: name.to_string(),
                size: [w.parse().ok()?, h.parse().ok()?],
                position: [x.parse().ok()?, y.parse().ok()?],
                refresh_hz: hz.parse().ok().filter(|r: &f32| *r > 0.0),
            })
        })
        .collect()
}

/// Child-process side: print one tab-separated line per monitor and exit.
pub fn print_list() -> anyhow::Result<()> {
    use winit::application::ApplicationHandler;
    use winit::event::{StartCause, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::window::WindowId;

    struct Query;
    impl ApplicationHandler for Query {
        fn new_events(&mut self, el: &ActiveEventLoop, cause: StartCause) {
            if cause != StartCause::Init {
                return;
            }
            for (i, m) in el.available_monitors().enumerate() {
                let name = m.name().unwrap_or_else(|| format!("Display {}", i + 1)).replace('\t', " ");
                let (s, p) = (m.size(), m.position());
                let hz = m.refresh_rate_millihertz().map_or(0.0, |r| r as f32 / 1000.0);
                println!("{name}\t{}\t{}\t{}\t{}\t{hz}", s.width, s.height, p.x, p.y);
            }
            el.exit();
        }
        fn resumed(&mut self, _: &ActiveEventLoop) {}
        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }
    EventLoop::new()?.run_app(&mut Query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_child_output() {
        let d = super::parse("HDMI-A-1\t1920\t1080\t0\t0\t60\nbad line\nDP-2\t3840\t2160\t1920\t0\t0\n");
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].label(), "HDMI-A-1 — 1920×1080 @ 60 Hz");
        assert_eq!(d[1].position, [1920, 0]);
        assert_eq!(d[1].refresh_hz, None);
    }
}
