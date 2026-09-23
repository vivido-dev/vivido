//! Bounded host text work, independent of control, input and UI event loops.
use super::*;
use vivid_protocol::overlay::wire::text::styled::{
    BatchMeasured, MAX_RETAINED_LAYOUTS, MeasureBatch, ReleaseLayouts,
};
use vivid_protocol::overlay::wire::text::{
    EditorGeometry, MAX_TEXT_GEOMETRY, MeasureText, TextGeometry, TextMeasurement,
};
use vivid_protocol::vector::{Rect, Text};

pub(in crate::vivid) fn measure_batch(
    shared: &Arc<ServiceShared>,
    session: &Arc<SessionRuntime>,
    record: &Record,
    request_id: u64,
    value: &Value,
) -> Result<(), ControlError> {
    if !session.supports(registry::OVERLAY_TEXT_LAYOUT) {
        return Err(ControlError::unsupported("overlay-text-layout-v1 was not negotiated"));
    }
    let request = MeasureBatch::decode(record.object_id, value)
        .map_err(|_| ControlError::bad_message("invalid text batch"))?;
    if request.texts.iter().any(|t| t.typography != Default::default())
        && !session.supports(registry::OVERLAY_TYPOGRAPHY)
    {
        return Err(ControlError::unsupported("overlay-typography-v1 was not negotiated"));
    }
    require_context_operation(session, request.address.context_id, OP_SURFACE_TRACK_MEDIA)?;
    let (font, id) = {
        let mut host = lock(&shared.overlays);
        let id = authorize(&host, session, request.address)?;
        host.layout_charges.retain(|_, token| token.strong_count() != 0);
        let charged =
            host.layout_charges.keys().filter(|(owner, _)| *owner == session.identity).count();
        if host.text_jobs.contains(&session.identity)
            || host.text_jobs.len() >= vivid_protocol::overlay::MAX_OWNERS
            || (request.retain && charged + request.texts.len() > MAX_RETAINED_LAYOUTS)
            || (request.retain
                && host.layout_charges.len() + request.texts.len()
                    > MAX_RETAINED_LAYOUTS * vivid_protocol::overlay::MAX_OWNERS)
        {
            return Err(ControlError::limit("text worker or retained layout capacity exceeded"));
        }
        host.text_jobs.insert(session.identity);
        (host.font(), id)
    };
    let worker_shared = shared.clone();
    let worker_session = session.clone();
    let spawned = thread::Builder::new().name("vivid-overlay-text-batch".into()).spawn(move || {
        let shared = worker_shared;
        let session = worker_session;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut system = TextSystem::new(font);
            let mut results = Vec::with_capacity(request.texts.len());
            for text in &request.texts {
                let prepared = system.prepare_styled(text);
                let layout = &prepared.layout;
                let source = prepared.text.text();
                let mut measured = layout_geometry(
                    layout,
                    &source,
                    text.max_lines.map_or(usize::MAX, usize::from),
                )?;
                if let Some(cut) = prepared.truncated_at {
                    measured.clusters.retain(|g| g.end as usize > prepared.prefix_bytes);
                    for geometry in measured.lines.iter_mut().chain(&mut measured.clusters) {
                        geometry.start = (geometry.start as usize)
                            .saturating_sub(prepared.prefix_bytes)
                            .min(cut) as u32;
                        geometry.end = (geometry.end as usize)
                            .saturating_sub(prepared.prefix_bytes)
                            .min(cut) as u32;
                    }
                    measured.truncated_at = Some(cut as u32);
                    measured
                        .validate_text(&text.text())
                        .map_err(|_| "invalid ellipsis source mapping")?;
                }
                if let Some(width) = text.max_width {
                    measured.width = width;
                }
                let painted = if request.retain {
                    Some(Arc::new(
                        crate::display::vector::text_layout::paint(
                            layout,
                            &prepared.text,
                            measured.width.get(),
                            measured.height.get(),
                        )
                        .map_err(|e| e.0)?,
                    ))
                } else {
                    None
                };
                results.push((measured, painted));
            }
            Ok::<_, &'static str>(results)
        }))
        .unwrap_or(Err("host text shaping failed"));
        // The context lock precedes the host lock, matching authorization elsewhere.
        let contexts = lock(&session.contexts);
        let context = contexts
            .get(&request.address.context_id)
            .filter(|c| c.operation_classes & OP_SURFACE_TRACK_MEDIA != 0);
        let mut host = lock(&shared.overlays);
        let response = (|| {
            let context = context.ok_or(ControlError::bad_state("text context was revoked"))?;
            authorize(&host, &session, request.address)?;
            let results = result.map_err(ControlError::limit)?;
            host.layout_charges.retain(|_, token| token.strong_count() != 0);
            if request.retain
                && host.layout_charges.len() + results.len()
                    > MAX_RETAINED_LAYOUTS * vivid_protocol::overlay::MAX_OWNERS
            {
                return Err(ControlError::limit("global retained layout capacity exceeded"));
            }
            let count = if request.retain { results.len() as u64 } else { 0 };
            let next = host
                .next_layout
                .checked_add(count)
                .ok_or(ControlError::limit("layout identities exhausted"))?;
            let first = host.next_layout;
            let layouts = results
                .iter()
                .enumerate()
                .map(|(i, (m, _))| {
                    (if request.retain { first + i as u64 + 1 } else { 0 }, m.clone())
                })
                .collect();
            let payload = BatchMeasured { address: request.address, layouts }
                .payload()
                .map_err(|_| ControlError::limit("text batch geometry limit"))?;
            let body = Envelope::new(request_id, payload)
                .encode()
                .map_err(|_| ControlError::limit("text batch encoding failed"))?;
            let limit = context
                .contract
                .get(Resource::ControlRecordBody)
                .min(u64::from(session.control_body_limit));
            if body.len() as u64 > limit {
                return Err(ControlError::limit("text batch exceeds reply limit"));
            }
            // Install the complete batch only after all shaping, geometry, and reply checks pass.
            for (i, (_, layout)) in results.into_iter().enumerate() {
                if let Some(layout) = layout {
                    let layout_id = first + i as u64 + 1;
                    host.layout_charges
                        .insert((session.identity, layout_id), Arc::downgrade(&layout.token));
                    host.layouts.insert((id, layout_id), layout);
                }
            }
            host.next_layout = next;
            Ok(body)
        })();
        host.text_jobs.remove(&session.identity);
        match response {
            Ok(body) => {
                session.post_control(
                    messages::OVERLAY_TEXT_BATCH_MEASURED,
                    request.address.surface_id,
                    body,
                );
            },
            Err(error) => {
                if let Ok(body) = protocol_error(request_id, error.code, false, error.message) {
                    session.post_control(messages::ERROR, request.address.surface_id, body);
                }
            },
        }
    });
    if spawned.is_err() {
        lock(&shared.overlays).text_jobs.remove(&session.identity);
        return Err(ControlError::limit("could not start text worker"));
    }
    Ok(())
}

pub(in crate::vivid) fn release_layouts(
    shared: &ServiceShared,
    session: &SessionRuntime,
    record: &Record,
    value: &Value,
) -> Result<(), ControlError> {
    if !session.supports(registry::OVERLAY_TEXT_LAYOUT) {
        return Err(ControlError::unsupported("overlay-text-layout-v1 was not negotiated"));
    }
    let request = ReleaseLayouts::decode(record.object_id, value)
        .map_err(|_| ControlError::bad_message("invalid layout release"))?;
    require_context_operation(session, request.address.context_id, OP_SURFACE_TRACK_MEDIA)?;
    let mut host = lock(&shared.overlays);
    let id = authorize(&host, session, request.address)?;
    if request.ids.iter().any(|layout| !host.layouts.contains_key(&(id, *layout))) {
        return Err(ControlError::not_found("text layout is absent or released"));
    }
    for layout in request.ids {
        host.layouts.remove(&(id, layout));
    }
    host.layout_charges.retain(|_, token| token.strong_count() != 0);
    Ok(())
}

fn authorize(
    host: &Host,
    session: &SessionRuntime,
    address: WindowAddress,
) -> Result<SurfaceIdentity, ControlError> {
    if !session.supports(registry::OVERLAY_TEXT) {
        return Err(ControlError::unsupported("overlay-text-v1 was not negotiated"));
    }
    let id = address
        .identity(session.identity)
        .map_err(|_| ControlError::bad_message("invalid text window"))?;
    let window =
        host.windows.get(id).ok_or_else(|| ControlError::not_found("overlay window is absent"))?;
    if window.generation != address.generation || !host.lanes.contains_key(&session.identity) {
        return Err(ControlError::precondition("stale text window or absent input lane"));
    }
    Ok(id)
}

pub(in crate::vivid) fn measure(
    shared: &Arc<ServiceShared>,
    session: &Arc<SessionRuntime>,
    record: &Record,
    request_id: u64,
    value: &Value,
) -> Result<(), ControlError> {
    let request = MeasureText::decode(record.object_id, value)
        .map_err(|_| ControlError::bad_message("invalid text measurement request"))?;
    require_context_operation(session, request.address.context_id, OP_SURFACE_TRACK_MEDIA)?;
    let font = {
        let mut host = lock(&shared.overlays);
        authorize(&host, session, request.address)?;
        if host.text_jobs.contains(&session.identity)
            || host.text_jobs.len() >= vivid_protocol::overlay::MAX_OWNERS
        {
            return Err(ControlError::limit("text measurement worker capacity exceeded"));
        }
        host.text_jobs.insert(session.identity);
        host.font()
    };
    let worker_shared = shared.clone();
    let worker_session = session.clone();
    let spawned = thread::Builder::new().name("vivid-overlay-text".into()).spawn(move || {
        let shared = worker_shared;
        let session = worker_session;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            measure_text(&mut TextSystem::new(font), &request.text)
        }))
        .unwrap_or(Err("host text shaping failed"))
        .and_then(|measurement| {
            measurement.payload(request.address).map_err(|_| "invalid text geometry")
        })
        .and_then(|payload| {
            Envelope::new(request_id, payload).encode().map_err(|_| "text reply encoding failed")
        });
        // Never publish a result for a revoked context, dismissed window, or lost lane.
        let authorized =
            require_context_operation(&session, request.address.context_id, OP_SURFACE_TRACK_MEDIA)
                .is_ok()
                && authorize(&lock(&shared.overlays), &session, request.address).is_ok();
        let result =
            if authorized { result } else { Err("text window was revoked during measurement") };
        let reply_limit = lock(&session.contexts)
            .get(&request.address.context_id)
            .map_or(0, |context| context.contract.get(Resource::ControlRecordBody))
            .min(u64::from(session.control_body_limit));
        let response = match result {
            Ok(body) if body.len() as u64 <= reply_limit => {
                Ok((messages::OVERLAY_TEXT_MEASURED, body))
            },
            _ => protocol_error(
                request_id,
                if authorized { messages::ERROR_LIMIT_EXCEEDED } else { messages::ERROR_BAD_STATE },
                false,
                "text measurement unavailable or exceeds reply limits",
            )
            .map(|body| (messages::ERROR, body)),
        };
        lock(&shared.overlays).text_jobs.remove(&session.identity);
        if let Ok((kind, body)) = response {
            session.post_control(kind, request.address.surface_id, body);
        }
    });
    if spawned.is_err() {
        lock(&shared.overlays).text_jobs.remove(&session.identity);
        return Err(ControlError::limit("could not start text measurement worker"));
    }
    Ok(())
}

pub(in crate::vivid) fn set_editor(
    shared: &ServiceShared,
    session: &SessionRuntime,
    record: &Record,
    value: &Value,
) -> Result<(), ControlError> {
    let request = EditorGeometry::decode(record.object_id, value)
        .map_err(|_| ControlError::bad_message("invalid editor geometry"))?;
    require_context_operation(session, request.address.context_id, OP_SURFACE_TRACK_MEDIA)?;
    let mut host = lock(&shared.overlays);
    let id = authorize(&host, session, request.address)?;
    if host.windows.focus() != Some(id)
        || host.windows.get(id).is_none_or(|w| w.scene_revision != request.scene_revision)
    {
        return Err(ControlError::precondition("editor requires focused presented scene"));
    }
    host.editor = request.caret.map(|_| (id, request));
    drop(host);
    shared.request_frame_wake();
    Ok(())
}

impl Host {
    /// Logical pane-local rectangle, clipped to both the owning window and viewport.
    pub(in crate::vivid) fn editor_rect(&mut self) -> Option<Rect> {
        let (id, request) = self.editor?;
        let valid = self.windows.focus() == Some(id)
            && self.windows.visible().any(|w| {
                w.identity == id
                    && w.generation == request.address.generation
                    && w.scene_revision == request.scene_revision
            });
        if !valid {
            self.editor = None;
            return None;
        }
        let bounds = self.windows.get(id)?.options.bounds;
        let caret = request.caret?;
        let viewport = self.viewport?;
        let x = (bounds.origin.x.get() + caret.origin.x.get()).max(bounds.origin.x.get()).max(0.);
        let y = (bounds.origin.y.get() + caret.origin.y.get()).max(bounds.origin.y.get()).max(0.);
        let right = (bounds.origin.x.get() + caret.origin.x.get() + caret.width.get())
            .min(bounds.origin.x.get() + bounds.width.get())
            .min(viewport.width.get());
        let bottom = (bounds.origin.y.get() + caret.origin.y.get() + caret.height.get())
            .min(bounds.origin.y.get() + bounds.height.get())
            .min(viewport.height.get());
        Rect::new(x, y, right - x, bottom - y).ok()
    }
}

pub(super) fn measure_text(
    system: &mut TextSystem,
    text: &Text,
) -> Result<TextMeasurement, &'static str> {
    let layout = system.shape_overlay(text);
    let mut measured = layout_geometry(&layout, &text.text, usize::MAX)?;
    measured.width =
        Scalar::new(if text.text.is_empty() { 0. } else { f64::from(layout.full_width()) })
            .map_err(|_| "text extent out of range")?;
    measured.height =
        Scalar::new(f64::from(layout.height())).map_err(|_| "text extent out of range")?;
    Ok(measured)
}

fn layout_geometry<B: parley::Brush>(
    layout: &parley::Layout<B>,
    source: &str,
    max_lines: usize,
) -> Result<TextMeasurement, &'static str> {
    let scalar = |v: f32| Scalar::new(f64::from(v)).map_err(|_| "text geometry out of range");
    let mut result = TextMeasurement {
        truncated_at: None,
        width: Scalar::ZERO,
        height: Scalar::ZERO,
        lines: Vec::new(),
        clusters: Vec::new(),
    };
    for line in layout.lines().take(max_lines) {
        if result.lines.len() >= MAX_TEXT_GEOMETRY {
            return Err("too many text lines");
        }
        let m = line.metrics();
        let range = line.text_range();
        let geometry = TextGeometry {
            start: range.start as u32,
            end: range.end.min(source.len()) as u32,
            x: scalar(m.offset)?,
            y: scalar(m.block_min_coord)?,
            width: if source.is_empty() { Scalar::ZERO } else { scalar(m.advance)? },
            height: scalar(m.block_max_coord - m.block_min_coord)?,
            baseline: scalar(m.baseline)?,
            rtl: false,
        };
        result.lines.push(geometry.clone());
        result.width = result.width.max(if source.is_empty() {
            Scalar::ZERO
        } else {
            scalar(m.offset + m.advance)?
        });
        result.height = scalar(m.block_max_coord)?;
        let mut x = m.offset;
        for run in line.runs() {
            for cluster in run.visual_clusters() {
                if result.clusters.len() >= MAX_TEXT_GEOMETRY {
                    return Err("too many text clusters");
                }
                let range = cluster.text_range();
                result.clusters.push(TextGeometry {
                    start: range.start as u32,
                    end: range.end as u32,
                    x: scalar(x)?,
                    width: scalar(cluster.advance())?,
                    baseline: Scalar::ZERO,
                    rtl: cluster.is_rtl(),
                    ..geometry.clone()
                });
                x += cluster.advance();
            }
        }
    }
    result.validate_text(source).map_err(|_| "invalid text ranges")?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn styled_layout_wraps_aligns_clips_and_preserves_unicode_style_boundaries() {
        use vivid_protocol::overlay::wire::text::styled::*;
        let mut system = TextSystem::new(Default::default());
        let mut text = StyledText::new(
            "one two three four five six",
            TextStyle { size: Scalar::new(18.).unwrap(), ..Default::default() },
        );
        text.max_width = Some(Scalar::new(90.).unwrap());
        let wrapped = system.shape_styled(&text);
        assert!(wrapped.len() > 1);
        let full = layout_geometry(&wrapped, &text.text(), usize::MAX).unwrap();
        let clipped = layout_geometry(&wrapped, &text.text(), 1).unwrap();
        assert_eq!(clipped.lines.len(), 1);
        assert!(clipped.height < full.height);
        text.runs[0].text = "A".into();
        text.wrap = false;
        text.alignment = TextAlignment::Center;
        let centered = system.shape_styled(&text);
        assert!(centered.get(0).unwrap().metrics().offset > 10.);
        text.runs.push(TextRun {
            text: "😀日".into(),
            style: TextStyle {
                size: Scalar::new(24.).unwrap(),
                weight: 700,
                underline: true,
                ..Default::default()
            },
        });
        let layout = system.shape_styled(&text);
        let geometry = layout_geometry(&layout, &text.text(), usize::MAX).unwrap();
        assert_eq!(geometry.clusters.iter().map(|c| c.end).max(), Some(8));
        let scene = crate::display::vector::text_layout::paint(
            &layout,
            &text,
            geometry.width.get(),
            geometry.height.get(),
        )
        .unwrap();
        assert!(!scene.scene.encoding().resources.glyphs.is_empty());
    }
    #[test]
    fn measurement_uses_paint_shaping_and_unicode_cluster_boundaries() {
        let mut system = TextSystem::new(Default::default());
        for content in ["", "hello world", "A😀日e\u{301}", "אבג test", "a\nb\nc"] {
            let text = Text {
                text: content.into(),
                origin: vivid_protocol::vector::Point::new(100., 200.).unwrap(),
                size: Scalar::new(18.).unwrap(),
                family: String::new(),
                weight: 400,
                italic: false,
                color: vivid_protocol::vector::Color(0xffffffff),
                max_width: Some(Scalar::new(50.).unwrap()),
            };
            let measured = measure_text(&mut system, &text).unwrap();
            let painted = system.shape_overlay(&text);
            assert!(
                (measured.width.get()
                    - if content.is_empty() { 0. } else { f64::from(painted.full_width()) })
                .abs()
                    < 1e-6
            );
            assert!((measured.height.get() - f64::from(painted.height())).abs() < 1e-6);
            measured.validate_text(content).unwrap();
            assert_eq!(measured.lines.len(), painted.len());
            if content.contains('א') {
                assert!(measured.clusters.iter().any(|c| c.rtl));
            }
        }
    }
    #[test]
    fn editor_is_focus_revision_owner_and_viewport_scoped() {
        use vivid_protocol::identity::PresenterInstanceId;
        use vivid_protocol::overlay::{WindowMode, WindowOptions};
        let first = SessionIdentity { presenter: PresenterInstanceId([1; 16]), session_id: 1 }
            .context(1)
            .unwrap()
            .surface(1)
            .unwrap();
        let other = SessionIdentity { session_id: 2, ..first.context.session }
            .context(1)
            .unwrap()
            .surface(1)
            .unwrap();
        let mut host = Host::default();
        host.update_viewport(800., 600., 2.).unwrap();
        for id in [first, other] {
            host.windows
                .create(
                    id,
                    1,
                    WindowOptions::new(
                        Rect::new(10., 20., 100., 80.).unwrap(),
                        WindowMode::Floating,
                    ),
                )
                .unwrap();
            host.windows.publish_scene(id, 1, 7).unwrap();
        }
        host.windows.request_focus(first).unwrap();
        let editor = EditorGeometry {
            address: WindowAddress { context_id: 1, surface_id: 1, generation: 1 },
            scene_revision: 7,
            caret: Some(Rect::new(95., 5., 20., 15.).unwrap()),
        };
        host.editor = Some((first, editor));
        assert_eq!(host.editor_rect(), Some(Rect::new(105., 25., 5., 15.).unwrap()));
        host.remove_owner(other.context.session);
        assert!(host.editor_rect().is_some());
        host.windows.publish_scene(first, 1, 8).unwrap();
        assert_eq!(host.editor_rect(), None);
        host.editor = Some((first, EditorGeometry { scene_revision: 8, ..editor }));
        host.set_pane_focus(false);
        host.set_pane_focus(true);
        assert_eq!(host.editor_rect(), None);
    }
}
