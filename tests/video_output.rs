//! The video output process end to end: layers shown by the real `opnq --video-output`
//! (snapshots read back from its GPU), YUV movies converted in the shader, and a crash of the
//! process brought back with its layers. Skipped where no display or GPU is available.
#![cfg(feature = "video")]

use opnq::model::{CameraCue, CueId, Stage, VideoCue};
use opnq::vout::link::{StageEvent, Status, VideoLink};
use opnq::vout::proto::Windows;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn link() -> Option<VideoLink> {
    // SAFETY: set before any thread of this test reads the environment.
    unsafe { std::env::set_var("OPNQ_VIDEO_EXE", env!("CARGO_BIN_EXE_opnq")) };
    opnq::media::video::init().ok()?;
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
    let d = std::env::temp_dir().join(format!("opnq-vout-test-{}", std::process::id()));
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

    // The process crashes: OpnQ starts a new one and the layer comes back.
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
    let lit = img.pixels().filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0).count();
    assert!(lit == 0, "output not black after stop: {lit} pixels lit, e.g. {:?}", img.pixels().find(|p| p[0] != 0 || p[1] != 0 || p[2] != 0));

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

/// A crash in the middle of things: a finished opacity fade and a looping movie come back as they
/// were (the fade's value, the loop's point), and an output set to close when nothing plays
/// closes after its last layer.
#[test]
fn fades_and_loops_survive_a_crash_and_idle_outputs_close() {
    let Some(mut link) = link() else { return };
    // (Not under `dir()`: the other test removes that when it ends.)
    let dir = std::env::temp_dir().join(format!("opnq-vout-fades-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut stage = Stage { width: 320, height: 180, close_when_idle: true, ..Stage::new("Fades") };
    link.set_stages(std::slice::from_ref(&stage));
    // Open, as the operator (or a layer, which the UI then lists as open) leaves it.
    link.set_windows(Windows { open: vec![stage.id], ..Default::default() });

    // Red at half opacity, by a fade that finished before the crash.
    let cam = CueId::new_v4();
    let c = CameraCue { format: Some("lavfi".into()), device: Some("color=red:size=64x36".into()), ..Default::default() };
    link.start_camera(cam, &c, Instant::now());
    let Some(shown) = wait(&mut link, 20, |l| l.frames(cam).is_some_and(|n| n > 2)) else { return };
    assert!(shown, "camera never showed: {:?}", link.drain_errors());
    // The geometry preview's live picture: small red copies.
    link.watch(Some(cam));
    let thumb = wait(&mut link, 10, |l| l.thumbnail(cam).is_some()).unwrap_or(false);
    assert!(thumb, "no live picture");
    let (_, t) = link.thumbnail(cam).unwrap();
    assert!(t.size[0] <= opnq::vout::proto::THUMB_SIDE && t.rgba.len() == (t.size[0] * t.size[1] * 4) as usize);
    assert_eq!(t.stage_size, [64.0, 36.0]);
    assert!(t.rgba[0] > 200 && t.rgba[1] < 60, "{:?}", &t.rgba[..4]);
    link.watch(None);
    assert!(link.thumbnail(cam).is_none());
    let fade = opnq::engine::FadeRequest {
        volume_db: None,
        opacity: Some(0.5),
        geometry: None,
        objects: None,
        video_fx: None,
        out_levels: None,
        audio_fx: None,
        duration: 0.2,
        curve: Default::default(),
    };
    link.fade(cam, &fade, Instant::now());

    // A 1 s movie looping.
    let movie = dir.join("loop.mp4");
    let made = std::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25:duration=1"])
        .args(["-pix_fmt", "yuv420p"])
        .arg(&movie)
        .status()
        .is_ok_and(|s| s.success());
    let looped = made.then(|| {
        let id = CueId::new_v4();
        let v = VideoCue { path: movie.clone(), infinite_loop: true, layer: -1, opacity: 0.0, ..Default::default() };
        let t0 = Instant::now();
        link.start(id, &v, t0, None).unwrap();
        (id, t0)
    });
    wait(&mut link, 3, |_| false);
    let px = centre(&snapshot(&mut link, &stage, &dir.join("half.png")));
    assert!((100..=155).contains(&px[0]) && px[1] < 30, "half red before the crash: {px:?}");

    link.crash();
    let restarted = wait(&mut link, 20, |l| matches!(l.status(), Status::Restarting(_) | Status::Starting)).unwrap_or(false);
    assert!(restarted, "crash not noticed");
    let back = wait(&mut link, 30, |l| l.status() == Status::Ready && l.frames(cam).is_some_and(|n| n > 2)).unwrap_or(false);
    assert!(back, "layer not restored: {:?}", link.drain_errors());
    let px = centre(&snapshot(&mut link, &stage, &dir.join("half-again.png")));
    assert!((100..=155).contains(&px[0]) && px[1] < 30, "the fade's value was lost in the restart: {px:?}");

    if let Some((id, t0)) = looped {
        assert_eq!(wait(&mut link, 10, |l| l.frames(id).is_some_and(|n| n > 5)), Some(true));
        // Positions come a few times a second: compare a fresh one with the loop's point.
        wait(&mut link, 1, |_| false);
        let pos = link.elapsed(id).unwrap();
        let expected = t0.elapsed().as_secs_f64() % 1.0;
        let off = (pos - expected).rem_euclid(1.0);
        assert!(off.min(1.0 - off) < 0.2, "loop point after the restart: at {pos:.2} s, expected {expected:.2} s");
        link.stop(id);
    }

    // Nothing left on the stage: its window closes by itself (opened again by the next layer).
    let mut closed = false;
    let mut opened = false;
    link.stop(cam);
    let r = wait(&mut link, 5, |l| {
        for e in l.take_stage_events() {
            closed |= e == StageEvent::IdleClosed(stage.id);
        }
        closed
    });
    assert_eq!(r, Some(true), "the idle output didn't close");
    // The UI takes it off its open list.
    link.set_windows(Windows::default());
    link.start_camera(cam, &c, Instant::now());
    let r = wait(&mut link, 10, |l| {
        for e in l.take_stage_events() {
            opened |= e == StageEvent::Opened(stage.id);
        }
        opened
    });
    assert_eq!(r, Some(true), "the output didn't open again for the next layer");
    stage.close_when_idle = false;
    link.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fine detail drawn much smaller than its size (a 4096 px stage in a smaller window, a still of
/// 1 px stripes) comes out even grey through the mip chains, not as aliased stripes.
#[test]
fn small_outputs_dont_alias() {
    let Some(mut link) = link() else { return };
    let dir = std::env::temp_dir().join(format!("opnq-vout-mips-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let stage = Stage { width: 4096, height: 2304, ..Stage::new("Big") };
    link.set_stages(std::slice::from_ref(&stage));
    let stripes = image::RgbaImage::from_fn(4096, 2304, |x, _| if x % 2 == 0 { image::Rgba([255, 255, 255, 255]) } else { image::Rgba([0, 0, 0, 255]) });
    let path = dir.join("stripes.png");
    stripes.save(&path).unwrap();
    let id = CueId::new_v4();
    link.start(id, &VideoCue { path, ..Default::default() }, Instant::now(), None).unwrap();
    let Some(shown) = wait(&mut link, 20, |l| l.frames(id).is_some_and(|n| n > 0)) else { return };
    assert!(shown, "still never showed: {:?}", link.drain_errors());
    wait(&mut link, 1, |_| false);
    let img = snapshot(&mut link, &stage, &dir.join("small.png"));
    assert!(img.width() < 2048, "the window isn't smaller than the stage ({} px)", img.width());
    let (w, h) = (img.width(), img.height());
    let row: Vec<f64> = (w / 4..3 * w / 4).map(|x| img.get_pixel(x, h / 2)[0] as f64).collect();
    let mean = row.iter().sum::<f64>() / row.len() as f64;
    let spread = row.iter().map(|v| (v - mean).abs()).fold(0.0, f64::max);
    assert!((100.0..=155.0).contains(&mean) && spread < 24.0, "aliased: mean {mean:.0}, spread {spread:.0}");
    link.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
