//! The video output process end to end: layers shown by the real `openq --video-output`
//! (snapshots read back from its GPU), YUV movies converted in the shader, and a crash of the
//! process brought back with its layers. Skipped where no display or GPU is available.
#![cfg(feature = "video")]

use openq::model::{CameraCue, CueId, Stage, VideoCue};
use openq::vout::link::{StageEvent, Status, VideoLink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn link() -> Option<VideoLink> {
    // SAFETY: set before any thread of this test reads the environment.
    unsafe { std::env::set_var("OPENQ_VIDEO_EXE", env!("CARGO_BIN_EXE_openq")) };
    openq::media::video::init().ok()?;
    VideoLink::new().ok()
}

/// Run the link until `f` holds (None: the video output can't run here).
fn wait(link: &mut VideoLink, secs: u64, mut f: impl FnMut(&mut VideoLink) -> bool) -> Option<bool> {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        link.update(Instant::now());
        if let Status::Unavailable(why) = link.status() {
            eprintln!("video output unavailable here: {why}");
            return None;
        }
        if f(link) {
            return Some(true);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Some(false)
}

fn snapshot(link: &mut VideoLink, stage: &Stage, path: &Path) -> image::RgbaImage {
    let _ = std::fs::remove_file(path);
    link.snapshot(stage.id, path.to_path_buf());
    let mut res = None;
    wait(link, 10, |l| {
        for e in l.take_stage_events() {
            if let StageEvent::Snapshot(r) = e {
                res = Some(r);
            }
        }
        res.is_some()
    });
    let p = res.expect("no snapshot").expect("snapshot failed");
    image::open(p).unwrap().to_rgba8()
}

fn centre(img: &image::RgbaImage) -> [u8; 3] {
    let p = img.get_pixel(img.width() / 2, img.height() / 2);
    [p[0], p[1], p[2]]
}

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("openq-vout-test-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn layers_show_survive_a_crash_and_go_black() {
    let Some(mut link) = link() else { return };
    let dir = dir();
    let stage = Stage { width: 320, height: 180, ..Stage::new("Test") };
    link.set_stages(std::slice::from_ref(&stage));

    // A camera test pattern: solid red.
    let cam = CueId::new_v4();
    let c = CameraCue { format: Some("lavfi".into()), device: Some("color=red:size=64x36".into()), ..Default::default() };
    link.start_camera(cam, &c, Instant::now());
    let Some(shown) = wait(&mut link, 20, |l| l.frames(cam).is_some_and(|n| n > 2)) else { return };
    assert!(shown, "camera never showed: {:?}", link.drain_errors());
    let img = snapshot(&mut link, &stage, &dir.join("red.png"));
    let px = centre(&img);
    assert!(px[0] > 200 && px[1] < 60 && px[2] < 60, "{px:?}");

    // The process crashes: OpenQ starts a new one and the layer comes back.
    link.crash();
    let restarted = wait(&mut link, 20, |l| matches!(l.status(), Status::Restarting(_) | Status::Starting)).unwrap_or(false);
    assert!(restarted, "crash not noticed");
    let back = wait(&mut link, 30, |l| l.status() == Status::Ready && l.frames(cam).is_some_and(|n| n > 2)).unwrap_or(false);
    assert!(back, "layer not restored after the crash: {:?}", link.drain_errors());
    let px = centre(&snapshot(&mut link, &stage, &dir.join("red-again.png")));
    assert!(px[0] > 200 && px[1] < 60, "{px:?}");

    // Stopped: the output is black, nothing else.
    link.stop(cam);
    let img = snapshot(&mut link, &stage, &dir.join("black.png"));
    assert!(img.pixels().all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0), "output not black after stop");

    // A YUV movie (limited-range BT.601 blue), converted on the GPU; it ends on its own.
    let movie = dir.join("blue.mp4");
    let made = std::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=0x0000ff:size=160x90:rate=25:duration=1"])
        .args(["-pix_fmt", "yuv420p"])
        .arg(&movie)
        .status()
        .is_ok_and(|s| s.success());
    if made {
        let id = CueId::new_v4();
        link.start(id, &VideoCue { path: movie, ..Default::default() }, Instant::now(), None).unwrap();
        assert_eq!(wait(&mut link, 10, |l| l.frames(id).is_some_and(|n| n > 3)), Some(true));
        let px = centre(&snapshot(&mut link, &stage, &dir.join("blue.png")));
        assert!(px[2] > 200 && px[0] < 40 && px[1] < 40, "{px:?}");
        assert_eq!(wait(&mut link, 10, |l| l.is_finished(id)), Some(true), "movie never finished");
        link.stop(id);
    }
    link.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
