use super::StrokePoint;

pub const DIRECTMAP_SIZE: usize = 48;
pub const DIRECTMAP_LEN: usize = 3 * DIRECTMAP_SIZE * DIRECTMAP_SIZE;
const TOMOE_BOX: i32 = 1000;
const RDP_ERROR_SQ: f32 = 225.0; // (1000 * 1000) / 4444

#[derive(Debug, Clone)]
pub struct StrokeStats {
    pub min_x: f32,
    pub max_x: f32,
    pub min_y: f32,
    pub max_y: f32,
    pub cx: f32,
    pub cy: f32,
    pub t_start: u64,
    pub t_end: u64,
}

impl StrokeStats {
    pub fn from_stroke(pts: &[StrokePoint]) -> Option<Self> {
        if pts.is_empty() {
            return None;
        }
        let mut min_x = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        let mut sx = 0.0f32;
        let mut sy = 0.0f32;
        let mut t_start = u64::MAX;
        let mut t_end = 0u64;
        for p in pts {
            if !p.x.is_finite() || !p.y.is_finite() {
                continue;
            }
            min_x = min_x.min(p.x);
            max_x = max_x.max(p.x);
            min_y = min_y.min(p.y);
            max_y = max_y.max(p.y);
            sx += p.x;
            sy += p.y;
            t_start = t_start.min(p.timestamp_ms);
            t_end = t_end.max(p.timestamp_ms);
        }
        if !min_x.is_finite() || !min_y.is_finite() {
            return None;
        }
        let n = pts.len() as f32;
        Some(Self {
            min_x,
            max_x,
            min_y,
            max_y,
            cx: sx / n,
            cy: sy / n,
            t_start,
            t_end,
        })
    }

    #[inline]
    pub fn width(&self) -> f32 {
        (self.max_x - self.min_x).max(0.0)
    }

    #[inline]
    pub fn height(&self) -> f32 {
        (self.max_y - self.min_y).max(0.0)
    }
}

pub fn global_bbox(strokes: &[Vec<StrokePoint>]) -> Option<(f32, f32, f32, f32)> {
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut any = false;
    for st in strokes {
        for p in st {
            if p.x.is_finite() && p.y.is_finite() {
                min_x = min_x.min(p.x);
                max_x = max_x.max(p.x);
                min_y = min_y.min(p.y);
                max_y = max_y.max(p.y);
                any = true;
            }
        }
    }
    if any {
        Some((min_x, max_x, min_y, max_y))
    } else {
        None
    }
}

/// Exact `points_normalized` from `mcyph/cnn_chinese_hw`:
/// independent min-max scaling into `[0..width] x [0..height]` with zero-span centering guard.
fn points_normalized_raw(
    strokes: &[Vec<(f32, f32)>],
    width: i32,
    height: i32,
    preserve_1d_center: bool,
) -> Vec<Vec<(i32, i32)>> {
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut count = 0usize;
    for st in strokes {
        for &(x, y) in st {
            if x.is_finite() && y.is_finite() {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
                count += 1;
            }
        }
    }
    if count == 0 {
        return Vec::new();
    }

    let dx = max_x - min_x;
    let dy = max_y - min_y;

    // For extreme 1D strokes (e.g. pure horizontal `一`, vertical `丨`, or single dot),
    // preserve centered alignment along the degenerate axis instead of pinning to 0.
    let (sx, sy, off_x, off_y) = if preserve_1d_center && dx < 1e-6 && dy < 1e-6 {
        (1.0f32, 1.0f32, (width as f32) * 0.5, (height as f32) * 0.5)
    } else if preserve_1d_center && strokes.len() == 1 && dx > 1e-6 && dy < 0.08 * dx {
        let sx = (width as f32) / dx;
        let target_h = (height as f32) * (dy / dx).max(0.02);
        let sy = if dy > 1e-6 { target_h / dy } else { 1.0 };
        let oy = ((height as f32) - target_h) * 0.5;
        (sx, sy, 0.0f32, oy)
    } else if preserve_1d_center && strokes.len() == 1 && dy > 1e-6 && dx < 0.08 * dy {
        let sy = (height as f32) / dy;
        let target_w = (width as f32) * (dx / dy).max(0.02);
        let sx = if dx > 1e-6 { target_w / dx } else { 1.0 };
        let ox = ((width as f32) - target_w) * 0.5;
        (sx, sy, ox, 0.0f32)
    } else {
        let sx = if dx > 1e-6 { (width as f32) / dx } else { 1.0 };
        let sy = if dy > 1e-6 { (height as f32) / dy } else { 1.0 };
        (sx, sy, 0.0f32, 0.0f32)
    };

    strokes
        .iter()
        .filter(|s| !s.is_empty())
        .map(|st| {
            st.iter()
                .filter(|(x, y)| x.is_finite() && y.is_finite())
                .map(|&(x, y)| {
                    let nx = ((x - min_x) * sx + off_x).round() as i32;
                    let ny = ((y - min_y) * sy + off_y).round() as i32;
                    (nx.clamp(0, width), ny.clamp(0, height))
                })
                .collect()
        })
        .collect()
}

/// Tomoe Ramer-Douglas-Peucker vertex simplification (`ERROR = 225` in `0..1000` space),
/// including the closed-loop split guard when `first == last`.
fn get_vertex(stroke: &[(i32, i32)]) -> Vec<(i32, i32)> {
    if stroke.len() <= 2 {
        return stroke.to_vec();
    }

    fn rdp(pts: &[(i32, i32)]) -> Vec<(i32, i32)> {
        if pts.len() <= 2 {
            return pts.to_vec();
        }
        let (x0, y0) = pts[0];
        let (x1, y1) = pts[pts.len() - 1];
        let vx = (x1 - x0) as f32;
        let vy = (y1 - y0) as f32;
        let denom = vx * vx + vy * vy;

        let mut max_d2 = -1.0f32;
        let mut max_idx = 0usize;
        for (i, &(px, py)) in pts.iter().enumerate().take(pts.len() - 1).skip(1) {
            let dx = (px - x0) as f32;
            let dy = (py - y0) as f32;
            let d2 = if denom == 0.0 {
                dx * dx + dy * dy
            } else {
                let cross = dx * vy - dy * vx;
                (cross * cross) / denom
            };
            if d2 > max_d2 {
                max_d2 = d2;
                max_idx = i;
            }
        }
        if max_d2 > RDP_ERROR_SQ {
            let mut left = rdp(&pts[..=max_idx]);
            let right = rdp(&pts[max_idx..]);
            left.pop();
            left.extend(right);
            left
        } else {
            vec![pts[0], pts[pts.len() - 1]]
        }
    }

    if stroke[0] == stroke[stroke.len() - 1] && stroke.len() >= 4 {
        let mid = stroke.len() / 2;
        let mut left = rdp(&stroke[..=mid]);
        let right = rdp(&stroke[mid..]);
        left.pop();
        left.extend(right);
        left
    } else {
        rdp(stroke)
    }
}

fn bresenham_line(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
    let mut pts = Vec::new();
    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx - dy;
    let mut cx = x0;
    let mut cy = y0;
    loop {
        pts.push((cx, cy));
        if cx == x1 && cy == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            cx += sx;
        }
        if e2 < dx {
            err += dx;
            cy += sy;
        }
    }
    pts
}

/// Render a stroke sequence into a `[3, 48, 48]` NCHW `f32` DirectMap tensor in `[0.0, 1.0]`.
pub fn render_directmap(strokes: &[Vec<StrokePoint>]) -> Vec<f32> {
    let raw_xy: Vec<Vec<(f32, f32)>> = strokes
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.iter().map(|p| (p.x, p.y)).collect())
        .collect();
    if raw_xy.is_empty() {
        return vec![0.0f32; DIRECTMAP_LEN];
    }

    let norm1000 = points_normalized_raw(&raw_xy, TOMOE_BOX, TOMOE_BOX, true);
    let simplified: Vec<Vec<(f32, f32)>> = norm1000
        .iter()
        .map(|st| {
            get_vertex(st)
                .into_iter()
                .map(|(x, y)| (x as f32, y as f32))
                .collect()
        })
        .collect();
    let norm47 = points_normalized_raw(
        &simplified,
        (DIRECTMAP_SIZE - 1) as i32,
        (DIRECTMAP_SIZE - 1) as i32,
        true,
    );

    let plane = DIRECTMAP_SIZE * DIRECTMAP_SIZE;
    let mut buf = vec![0u8; 3 * plane];
    let m = norm47.len();
    for (j, st) in norm47.iter().enumerate() {
        if st.is_empty() {
            continue;
        }
        let strokes_pc = if m > 1 {
            (j as f32) / ((m - 1) as f32)
        } else {
            1.0
        };
        let c2 = ((1.0 - strokes_pc) * 255.0) as u8;

        if st.len() == 1 {
            let (x, y) = st[0];
            let idx = (y as usize) * DIRECTMAP_SIZE + (x as usize);
            buf[idx] = 255;
            buf[plane + idx] = 255;
            buf[2 * plane + idx] = buf[2 * plane + idx].max(c2);
            continue;
        }

        let mut pix: Vec<(i32, i32)> = Vec::new();
        for w in st.windows(2) {
            let seg = bresenham_line(w[0].0, w[0].1, w[1].0, w[1].1);
            if !pix.is_empty() && seg.first() == pix.last() {
                pix.extend_from_slice(&seg[1..]);
            } else {
                pix.extend_from_slice(&seg);
            }
        }
        let l = pix.len();
        for (i, &(x, y)) in pix.iter().enumerate() {
            let stroke_pc = if l > 1 {
                (i as f32) / ((l - 1) as f32)
            } else {
                1.0
            };
            let c0 = ((0.2 + (1.0 - stroke_pc) * 0.8) * 255.0) as u8;
            let c1 = ((0.2 + stroke_pc * 0.8) * 255.0) as u8;
            let idx = (y as usize) * DIRECTMAP_SIZE + (x as usize);
            if c0 > buf[idx] {
                buf[idx] = c0;
            }
            if c1 > buf[plane + idx] {
                buf[plane + idx] = c1;
            }
            if c2 > buf[2 * plane + idx] {
                buf[2 * plane + idx] = c2;
            }
        }
    }

    buf.into_iter().map(|v| (v as f32) / 255.0).collect()
}

/// Reorder strokes into canonical Chinese top-left-to-bottom-right stroke order
/// (`0.7 * mean_y + 0.3 * mean_x`) for Mode 1 reversed-stroke-order invariance.
pub fn reorder_strokes_top_left(strokes: &[Vec<StrokePoint>]) -> Vec<Vec<StrokePoint>> {
    let mut keyed: Vec<(f32, usize)> = strokes
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.is_empty())
        .map(|(idx, s)| {
            let stats = StrokeStats::from_stroke(s);
            let key = match stats {
                Some(st) => st.cy * 0.7 + st.cx * 0.3,
                None => idx as f32,
            };
            (key, idx)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    keyed.into_iter().map(|(_, idx)| strokes[idx].clone()).collect()
}

/// 8-directional trajectory + spatial-zone descriptor for pure-Rust fallback & 1D stroke guards.
#[derive(Debug, Clone)]
pub struct GlyphGeometry {
    pub stroke_count: usize,
    pub aspect_ratio: f32,
    pub dir_hist: [f32; 8],
    pub zone_hist: [f32; 9],
    pub is_single_dot: bool,
    pub is_single_horizontal: bool,
    pub is_single_vertical: bool,
    pub is_single_pie: bool,
    pub is_single_na: bool,
}

impl GlyphGeometry {
    pub fn from_strokes(strokes: &[Vec<StrokePoint>]) -> Self {
        let non_empty: Vec<&Vec<StrokePoint>> =
            strokes.iter().filter(|s| !s.is_empty()).collect();
        let stroke_count = non_empty.len();
        let Some((min_x, max_x, min_y, max_y)) = global_bbox(strokes) else {
            return Self {
                stroke_count: 0,
                aspect_ratio: 1.0,
                dir_hist: [0.0; 8],
                zone_hist: [0.0; 9],
                is_single_dot: false,
                is_single_horizontal: false,
                is_single_vertical: false,
                is_single_pie: false,
                is_single_na: false,
            };
        };
        let dx = (max_x - min_x).max(1e-6);
        let dy = (max_y - min_y).max(1e-6);
        let aspect_ratio = dx / dy;

        let mut dir_hist = [0.0f32; 8];
        let mut total_len = 0.0f32;
        let mut zone_hist = [0.0f32; 9];
        let mut total_pts = 0.0f32;

        for st in &non_empty {
            for p in *st {
                let nx = ((p.x - min_x) / dx).clamp(0.0, 0.999);
                let ny = ((p.y - min_y) / dy).clamp(0.0, 0.999);
                let zx = (nx * 3.0) as usize;
                let zy = (ny * 3.0) as usize;
                zone_hist[zy * 3 + zx] += 1.0;
                total_pts += 1.0;
            }
            for w in st.windows(2) {
                let vx = w[1].x - w[0].x;
                let vy = w[1].y - w[0].y;
                let len = (vx * vx + vy * vy).sqrt();
                if len > 1e-6 {
                    let angle = vy.atan2(vx); // -pi..pi
                    let bin = (((angle + std::f32::consts::PI + std::f32::consts::FRAC_PI_8)
                        / (2.0 * std::f32::consts::PI))
                        * 8.0)
                        .floor() as usize
                        % 8;
                    dir_hist[bin] += len;
                    total_len += len;
                }
            }
        }
        if total_len > 0.0 {
            for v in &mut dir_hist {
                *v /= total_len;
            }
        }
        if total_pts > 0.0 {
            for v in &mut zone_hist {
                *v /= total_pts;
            }
        }

        let mut is_single_dot = false;
        let mut is_single_horizontal = false;
        let mut is_single_vertical = false;
        let mut is_single_pie = false;
        let mut is_single_na = false;

        if stroke_count == 1 {
            let st = non_empty[0];
            if st.len() == 1 || (max_x - min_x < 1e-4 && max_y - min_y < 1e-4) {
                is_single_dot = true;
            } else {
                let sx = st.last().unwrap().x - st.first().unwrap().x;
                let sy = st.last().unwrap().y - st.first().unwrap().y;
                let adx = sx.abs();
                let ady = sy.abs();
                let chord_len = (sx * sx + sy * sy).sqrt();
                let is_straight = total_len <= 1e-6 || chord_len >= 0.85 * total_len;
                if adx > 4.0 * ady.max(1e-6) && dy <= 0.25 * dx && is_straight {
                    is_single_horizontal = true;
                } else if ady > 4.0 * adx.max(1e-6) && dx <= 0.25 * dy && is_straight {
                    is_single_vertical = true;
                } else if sx < 0.0 && sy > 0.0 && is_straight {
                    is_single_pie = true;
                } else if sx > 0.0 && sy > 0.0 && adx + ady > 0.05 && is_straight {
                    is_single_na = true;
                }
            }
        }

        Self {
            stroke_count,
            aspect_ratio,
            dir_hist,
            zone_hist,
            is_single_dot,
            is_single_horizontal,
            is_single_vertical,
            is_single_pie,
            is_single_na,
        }
    }
}
