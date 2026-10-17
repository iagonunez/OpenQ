//! The light thread's frame must not allocate once a show is loaded and running (allocation
//! can take locks inside the allocator and its time is unbounded). Counted per thread with a
//! wrapping global allocator.

use openq::light::library::bench_rig;
use openq::light::playback::{LightShow, Playback};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Counting;

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn light_frame_does_not_allocate() {
    use openq::light::effects::{Effect, EffectRun, Shape};
    let mut rig = bench_rig(300, 3, 5);
    // Effects running too: a chase in sequence 1, a circle in sequence 2, a table in sequence 3.
    rig.effects.push(Effect { id: 1, shape: Shape::Square, ..Default::default() });
    rig.effects.push(Effect { id: 2, shape: Shape::Circle, attribute: openq::light::Attribute::Pan, relative: true, size: 0.2, ..Default::default() });
    let all: Vec<u32> = rig.fixtures.iter().map(|f| f.id).collect();
    rig.sequences[0].cues[0].effects.push(EffectRun { effect: 1, fixtures: all.clone(), stop: false });
    rig.sequences[1].cues[1].effects.push(EffectRun { effect: 2, fixtures: all.clone(), stop: false });
    // A custom (table) effect on colour in sequence 3.
    let attributes = vec![openq::light::Attribute::Red, openq::light::Attribute::Blue];
    let values = (0..2 * 16 * 4).map(|i| (i % 7) as f32 / 7.0).collect();
    let table = openq::light::effects::EffectTable { attributes, steps: 16, width: 4, smooth: true, values };
    rig.effects.push(Effect { id: 3, shape: Shape::Custom, table: Some(Arc::new(table)), ..Default::default() });
    rig.sequences[2].cues[0].effects.push(EffectRun { effect: 3, fixtures: all, stop: false });
    let show = Arc::new(LightShow::compile(&rig));
    let t0 = Instant::now();
    let mut p = Playback::new(show, t0);
    p.set_programmer(&[(1, openq::light::Attribute::Dimmer, 0.5)]);
    for s in 1..=3 {
        p.go(s, None, t0).unwrap();
    }
    p.tick(t0); // first frame may size buffers
    // A second round of GOs reuses the track buffers.
    for s in 1..=3 {
        p.go(s, None, t0).unwrap();
    }
    p.release(2, 1.0, t0);
    p.tick(t0);
    let mut status = p.status();
    let before = ALLOCS.with(|c| c.get());
    for i in 0..500 {
        p.tick(t0 + Duration::from_millis(i * 23));
        if i % 4 == 0 {
            p.status_into(&mut status);
        }
    }
    assert_eq!(ALLOCS.with(|c| c.get()) - before, 0, "allocations in steady-state frames");
    // GO into a cue of the same size: no allocation either.
    let before = ALLOCS.with(|c| c.get());
    p.go(1, None, t0).unwrap();
    p.tick(t0);
    assert_eq!(ALLOCS.with(|c| c.get()) - before, 0, "allocations in a GO");
}
