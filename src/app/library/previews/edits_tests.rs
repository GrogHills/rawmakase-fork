use super::*;
use crate::app::library::previews::tests::{edited_png, render_job};
use std::sync::mpsc::Receiver;

/// Renders that only count themselves, for a job taken from `shared`.
fn counting(
    shared: &Arc<Shared>,
    cache: Option<PreviewCache>,
    wanted: Wanted,
) -> (Renders, Receiver<EditResult>) {
    fn render(_: &Path, _: &EditSource, _: Cancel<'_>) -> anyhow::Result<image::RgbImage> {
        RENDERED.fetch_add(1, Ordering::SeqCst);
        Ok(image::RgbImage::new(4, 4))
    }
    let (tx, rx) = mpsc::channel();
    let renders = Renders {
        background: None,
        full: None,
        cache: Arc::new(Mutex::new(cache)),
        cache_path: PathBuf::new(),
        wanted,
        results: Results {
            tx,
            shared: shared.clone(),
            ctx: egui::Context::default(),
        },
        render,
        render_ai: render_edited_with_ai,
    };
    (renders, rx)
}

static RENDERED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[test]
fn photos_asked_ahead_are_not_rendered_once_they_lost_their_reason() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = edited_png(directory.path(), "1.png")?;
    let queued = Queued::new;
    // No cache to keep it in: rendered when shown.
    let shared = Arc::<Shared>::default();
    shared.lock().pace = Pace::Full;
    shared.lock().jobs.push(queued(render_job(1, &path, true)));
    let (renders, rx) = counting(&shared, None, Wanted::default());
    renders.start(0);
    assert!(matches!(rx.try_recv(), Ok(EditResult::Kept(PhotoId(1), 1))));
    // Looked up at full pace, started once Develop came to the front.
    let shared = Arc::<Shared>::default();
    shared.lock().jobs.push(queued(render_job(2, &path, true)));
    let cache = PreviewCache::open(&directory.path().join("previews.sqlite3"))?;
    let (renders, rx) = counting(&shared, Some(cache), Wanted::default());
    renders.start(0);
    assert!(matches!(
        rx.try_recv(),
        Ok(EditResult::Skipped(PhotoId(2), 2))
    ));
    assert!(shared.lock().running.is_empty());
    // Its list gone while it was looked up (a miss), before its render began.
    let shared = Arc::<Shared>::default();
    let queue = EditQueue(shared.clone());
    queue.set_pace(Pace::Full);
    queue.send(render_job(4, &path, true))?;
    assert!(matches!(
        next_step(&shared, &Wanted::default(), 2),
        Step::Look(0, true)
    ));
    queue.drop_ahead();
    let cache = PreviewCache::open(&directory.path().join("empty.sqlite3"))?;
    let (renders, rx) = counting(&shared, Some(cache), Wanted::default());
    renders.look(0, true);
    assert!(matches!(
        rx.try_recv(),
        Ok(EditResult::Skipped(PhotoId(4), 4))
    ));
    assert!(shared.lock().running.is_empty());
    drop(queue);
    // Closed since it was looked up.
    let shared = Arc::<Shared>::default();
    shared.lock().jobs.push(queued(render_job(3, &path, false)));
    let (renders, rx) = counting(&shared, None, Wanted::default());
    shared.close();
    renders.start(0);
    assert!(rx.try_recv().is_err());
    assert!(shared.lock().running.is_empty());
    assert_eq!(RENDERED.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
fn photos_asked_ahead_for_a_list_gone_are_dropped_unless_shown() {
    let directory = Path::new("/nonexistent");
    let (queue, _) = EditQueue::detached();
    let wanted = Wanted::default();
    wanted.lock().unwrap().insert(PhotoId(2));
    queue.set_pace(Pace::Full);
    for (id, ahead) in [(1, true), (2, true)] {
        queue.send(render_job(id, directory, ahead)).unwrap();
    }
    queue.drop_ahead();
    // Sent after the list changed: kept.
    queue.send(render_job(3, directory, true)).unwrap();
    // Not rendered, even at full pace; one shown since is, as a photo shown.
    assert!(matches!(
        next_step(&queue.0, &wanted, 2),
        Step::Skip(PhotoId(1), 1)
    ));
    assert!(matches!(
        next_step(&queue.0, &wanted, 2),
        Step::Look(0, true)
    ));
    queue.0.lock().jobs.remove(0);
    assert!(matches!(
        next_step(&queue.0, &wanted, 2),
        Step::Look(0, true)
    ));
}

/// A file being rendered isn't rendered again beside it, so the two share
/// one Camera Look fit rather than each making it. It is still looked up,
/// and other files render.
#[test]
fn computers_with_less_memory_render_fewer_side_by_side() {
    const GIB: u64 = 1 << 30;
    let renders = |gib: f64| full_renders(Some((gib * GIB as f64) as u64), 17);
    assert_eq!(renders(8.), 2);
    // Less than the label says, as systems keep some back.
    assert_eq!(renders(7.6), 2);
    assert_eq!(renders(12.), 3);
    assert_eq!(renders(16.), 4);
    assert_eq!(renders(64.), 4);
    assert_eq!(renders(4.), 1);
    assert_eq!(renders(1.), 1);
    // Unknown: as an 8 GB computer.
    assert_eq!(full_renders(None, 17), 2);
    // Never more than the full pace's threads.
    assert_eq!(full_renders(Some(64 * GIB), 2), 2);
}

#[test]
fn a_file_is_rendered_once_at_a_time() {
    let (one, other) = (Path::new("/photos/one.NEF"), Path::new("/photos/other.NEF"));
    let wanted = Wanted::default();
    wanted
        .lock()
        .unwrap()
        .extend([PhotoId(1), PhotoId(2), PhotoId(3)]);
    let queued = Queued::new;
    let shared = Shared::default();
    {
        let mut state = shared.lock();
        state.pace = Pace::Full;
        // A virtual copy of `one` is rendering.
        state.running.push(Running {
            slot: 0,
            full: true,
            ai: false,
            cancel: Arc::default(),
            path: one.into(),
        });
        state.jobs = vec![
            queued(render_job(2, other, false)),
            queued(render_job(3, one, false)),
        ];
    }
    // The latest, another copy of `one`: from the cache only.
    assert!(matches!(
        next_step(&shared, &wanted, 4),
        Step::Look(1, false)
    ));
    shared.lock().jobs[1].missed = true;
    // `other` renders beside it.
    assert!(matches!(
        next_step(&shared, &wanted, 4),
        Step::Look(0, true)
    ));
    shared.lock().jobs[0].missed = true;
    assert!(matches!(next_step(&shared, &wanted, 4), Step::Render(0)));
    // Once `one`'s render is done, its other copy renders.
    shared.lock().jobs.remove(0);
    shared.lock().running.clear();
    assert!(matches!(next_step(&shared, &wanted, 4), Step::Render(0)));
}

#[test]
fn photos_shown_go_before_the_ones_asked_ahead() {
    let directory = Path::new("/nonexistent");
    let shared = Shared::default();
    let wanted = Wanted::default();
    wanted.lock().unwrap().extend([PhotoId(2), PhotoId(3)]);
    let queued = Queued::new;
    {
        let mut state = shared.lock();
        state.pace = Pace::Full;
        state.jobs = vec![
            queued(render_job(1, directory, true)),
            queued(render_job(2, directory, false)),
            // Asked ahead and shown since: as a photo shown.
            queued(render_job(3, directory, true)),
            queued(render_job(4, directory, true)),
        ];
    }
    // The latest photo shown first, then the one before it.
    assert!(matches!(
        next_step(&shared, &wanted, 2),
        Step::Look(2, true)
    ));
    shared.lock().jobs[2].missed = true;
    assert!(matches!(
        next_step(&shared, &wanted, 2),
        Step::Look(1, true)
    ));
    shared.lock().jobs[1].missed = true;
    assert!(matches!(next_step(&shared, &wanted, 2), Step::Render(2)));
    shared.lock().jobs.remove(2);
    shared.lock().jobs.remove(1);
    // Then the ones asked ahead, in the order asked.
    assert!(matches!(
        next_step(&shared, &wanted, 2),
        Step::Look(0, true)
    ));
    // A photo out of view is answered without a render; at the background
    // pace so are the ones asked ahead.
    shared
        .lock()
        .jobs
        .push(queued(render_job(5, directory, false)));
    assert!(matches!(
        next_step(&shared, &wanted, 2),
        Step::Skip(PhotoId(5), 5)
    ));
    shared.lock().pace = Pace::Background;
    assert!(matches!(
        next_step(&shared, &wanted, 2),
        Step::Skip(PhotoId(1), 1)
    ));
    // With every slot taken a photo shown is still looked up.
    let mut state = shared.lock();
    state.jobs.clear();
    state.jobs.push(queued(render_job(2, directory, false)));
    state.running.push(Running {
        slot: 0,
        full: false,
        ai: false,
        cancel: Arc::default(),
        path: PathBuf::from("/elsewhere.png"),
    });
    drop(state);
    assert!(matches!(
        next_step(&shared, &wanted, 2),
        Step::Look(0, false)
    ));
}

#[test]
fn ai_renders_are_bounded_by_scheduler_even_with_four_slots() {
    fn unexpected_render(
        _: &Path,
        _: &EditSource,
        _: Cancel<'_>,
    ) -> anyhow::Result<image::RgbImage> {
        panic!("scheduler must not start a third AI render while two are active")
    }
    let directory = tempfile::tempdir().unwrap();
    let shared = Arc::<Shared>::default();
    let wanted = Wanted::default();
    wanted.lock().unwrap().extend([PhotoId(1), PhotoId(2)]);
    let mut ai_recipe = crate::model::recipe::Recipe::default();
    ai_recipe.ai_denoise.enabled = true;
    ai_recipe.ai_denoise.amount = 0.5;
    let ai_job = EditJob::Render {
        id: PhotoId(2),
        ticket: 2,
        path: directory.path().join("ai.ARW"),
        source: EditSource::Recipe(serde_json::to_string(&ai_recipe).unwrap()),
        ahead: false,
    };
    // Enqueueing stays O(1) without deserializing the recipe on the UI thread.
    let queued_plain = Queued::new(render_job(1, &directory.path().join("plain.png"), false));
    let queued_ai = Queued::new(ai_job);
    assert_eq!(queued_plain.renders_ai, None);
    assert_eq!(queued_ai.renders_ai, None);
    {
        let mut state = shared.lock();
        state.pace = Pace::Full;
        state.ai = Some(AiCache::new(directory.path().join("ai"), u64::MAX));
        for slot in 0..MAX_CONCURRENT_AI_RENDERS {
            state.running.push(Running {
                slot: slot as u64,
                full: true,
                ai: true,
                cancel: Arc::default(),
                path: directory.path().join(format!("busy-{slot}.ARW")),
            });
        }
        state.jobs = vec![queued_plain, queued_ai];
    }
    let (tx, _rx) = mpsc::channel();
    let renders = Renders {
        background: None,
        full: None,
        cache: Arc::new(Mutex::new(None)),
        cache_path: PathBuf::new(),
        wanted: wanted.clone(),
        results: Results {
            tx,
            shared: shared.clone(),
            ctx: egui::Context::default(),
        },
        render: unexpected_render,
        render_ai: render_edited_with_ai,
    };
    // Latest visible job has not been classified yet: look checks the cache and classifies
    // outside `State`'s mutex, then holds it at `missed` without starting a 3rd AI render.
    assert!(matches!(
        next_step(&shared, &wanted, 4),
        Step::Look(1, true)
    ));
    renders.look(1, true);
    {
        let state = shared.lock();
        assert_eq!(state.jobs.len(), 2);
        assert_eq!(state.running.len(), MAX_CONCURRENT_AI_RENDERS);
        assert_eq!(state.jobs[1].renders_ai, Some(true));
        assert!(state.jobs[1].missed);
    }
    // The non-AI job still looks up and renders beside the two AI renders.
    assert!(matches!(
        next_step(&shared, &wanted, 4),
        Step::Look(0, true)
    ));
    // Classify synchronously via look(..., false) before verifying next_step picks Render(0).
    renders.look(0, false);
    {
        let state = shared.lock();
        assert_eq!(state.jobs[0].renders_ai, Some(false));
        assert!(state.jobs[0].missed);
    }
    assert!(matches!(next_step(&shared, &wanted, 4), Step::Render(0)));
    // Once the non-AI job leaves and one AI slot frees up, the waiting AI job renders.
    shared.lock().jobs.remove(0);
    shared.lock().running.pop();
    assert!(matches!(next_step(&shared, &wanted, 4), Step::Render(0)));
}

#[test]
fn offscreen_ai_request_is_classified_before_lookup_when_ai_slots_full() {
    fn unexpected_render(
        _: &Path,
        _: &EditSource,
        _: Cancel<'_>,
    ) -> anyhow::Result<image::RgbImage> {
        panic!("unexpected render")
    }
    let directory = tempfile::tempdir().unwrap();
    let shared = Arc::<Shared>::default();
    let wanted = Wanted::default();
    let mut ai_recipe = crate::model::recipe::Recipe::default();
    ai_recipe.ai_denoise.enabled = true;
    ai_recipe.ai_denoise.amount = 0.5;
    let ai_job = EditJob::Render {
        id: PhotoId(1),
        ticket: 1,
        path: directory.path().join("ahead-ai.ARW"),
        source: EditSource::Recipe(serde_json::to_string(&ai_recipe).unwrap()),
        ahead: true,
    };
    let plain_job = render_job(2, &directory.path().join("ahead-plain.png"), true);
    {
        let mut state = shared.lock();
        state.pace = Pace::Full;
        state.ai = Some(AiCache::new(directory.path().join("ai"), u64::MAX));
        for slot in 0..MAX_CONCURRENT_AI_RENDERS {
            state.running.push(Running {
                slot: slot as u64,
                full: true,
                ai: true,
                cancel: Arc::default(),
                path: directory.path().join(format!("busy-{slot}.ARW")),
            });
        }
        state.jobs = vec![Queued::new(ai_job), Queued::new(plain_job)];
    }
    let (tx, _rx) = mpsc::channel();
    let renders = Renders {
        background: None,
        full: None,
        cache: Arc::new(Mutex::new(None)),
        cache_path: PathBuf::new(),
        wanted: wanted.clone(),
        results: Results {
            tx,
            shared: shared.clone(),
            ctx: egui::Context::default(),
        },
        render: unexpected_render,
        render_ai: render_edited_with_ai,
    };
    // Both AI slots are full: classify offscreen AI job without doing file I/O, then defer it.
    assert!(matches!(next_step(&shared, &wanted, 4), Step::Classify(0)));
    renders.classify(0);
    {
        let state = shared.lock();
        assert_eq!(state.jobs[0].renders_ai, Some(true));
        assert!(!state.jobs[0].missed);
    }
    // The deferred AI job is skipped; the plain offscreen job is classified then looked up.
    assert!(matches!(next_step(&shared, &wanted, 4), Step::Classify(1)));
    renders.classify(1);
    assert_eq!(shared.lock().jobs[1].renders_ai, Some(false));
    assert!(matches!(
        next_step(&shared, &wanted, 4),
        Step::Look(1, true)
    ));
    // Once the plain job is handled and an AI slot opens, the offscreen AI job proceeds to Look.
    shared.lock().jobs.remove(1);
    shared.lock().running.pop();
    assert!(matches!(
        next_step(&shared, &wanted, 4),
        Step::Look(0, true)
    ));
}

#[test]
fn set_shown_wakes_waiting_scheduler_for_deferred_offscreen_ai_job() {
    fn unexpected_render(
        _: &Path,
        _: &EditSource,
        _: Cancel<'_>,
    ) -> anyhow::Result<image::RgbImage> {
        panic!("unexpected render")
    }
    let directory = tempfile::tempdir().unwrap();
    let shared = Arc::<Shared>::default();
    let wanted = Wanted::default();
    let mut ai_recipe = crate::model::recipe::Recipe::default();
    ai_recipe.ai_denoise.enabled = true;
    ai_recipe.ai_denoise.amount = 0.5;
    let ai_job = EditJob::Render {
        id: PhotoId(1),
        ticket: 1,
        path: directory.path().join("ahead-ai.ARW"),
        source: EditSource::Recipe(serde_json::to_string(&ai_recipe).unwrap()),
        ahead: true,
    };
    {
        let mut state = shared.lock();
        state.pace = Pace::Full;
        state.ai = Some(AiCache::new(directory.path().join("ai"), u64::MAX));
        for slot in 0..MAX_CONCURRENT_AI_RENDERS {
            state.running.push(Running {
                slot: slot as u64,
                full: true,
                ai: true,
                cancel: Arc::default(),
                path: directory.path().join(format!("busy-{slot}.ARW")),
            });
        }
        state.jobs = vec![Queued::new(ai_job)];
    }
    let (tx, _rx) = mpsc::channel();
    let renders = Renders {
        background: None,
        full: None,
        cache: Arc::new(Mutex::new(None)),
        cache_path: PathBuf::new(),
        wanted: wanted.clone(),
        results: Results {
            tx,
            shared: shared.clone(),
            ctx: egui::Context::default(),
        },
        render: unexpected_render,
        render_ai: render_edited_with_ai,
    };
    renders.classify(0);
    assert_eq!(shared.lock().jobs[0].renders_ai, Some(true));
    assert!(!shared.lock().jobs[0].missed);

    let (step_tx, step_rx) = mpsc::channel();
    let started = Arc::new(AtomicBool::new(false));
    let shared_worker = shared.clone();
    let wanted_worker = wanted.clone();
    let started_worker = started.clone();
    let handle = std::thread::spawn(move || {
        started_worker.store(true, Ordering::SeqCst);
        let step = next_step(&shared_worker, &wanted_worker, 4);
        let _ = step_tx.send(matches!(step, Step::Look(0, false)));
    });
    while !started.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    // Give the worker thread a moment to enter `shared.changed.wait(state)`.
    std::thread::sleep(std::time::Duration::from_millis(15));
    assert!(step_rx.try_recv().is_err());

    let queue = EditQueue(shared.clone());
    queue.set_shown(&wanted, std::collections::HashSet::from([PhotoId(1)]));
    assert_eq!(
        step_rx.recv_timeout(std::time::Duration::from_secs(5)),
        Ok(true)
    );
    handle.join().unwrap();
    shared.lock().running.clear();
    drop(queue);
}
