//! 页面观察：把控件树精简成编号列表，并在截图上标号（行为与技术验证里已验证过的观察器一致）。
//!
//! Hafa 是 Flutter 应用，控件树里可读信息在 content-desc，所以列表里优先展示 content-desc。
//! 编号用 5×7 点阵自己画，不依赖字体文件。

use std::io::Cursor;

use device::Element;
use image::{imageops::FilterType, DynamicImage, ImageFormat, Rgb, RgbImage};

/// 宽度占屏幕 80% 以上的可点节点视为「宽节点」。
const WIDE_RATIO: f64 = 0.8;
const RADIUS: i32 = 22;
const RED: Rgb<u8> = Rgb([255, 60, 60]);
const WHITE: Rgb<u8> = Rgb([255, 255, 255]);

/// 元素的可读名字：优先 content-desc，其次 text，都没有就用类名。
pub fn label(el: &Element) -> String {
    let l = el.label().replace('\n', " / ").trim().to_string();
    if !l.is_empty() {
        return l;
    }
    el.class.rsplit('.').next().filter(|c| !c.is_empty()).unwrap_or("元素").to_string()
}

/// 生成编号列表。
///
/// Flutter 的语义节点有时横跨整行，而真正可点的图标只占其中一小块（例如 Mine 页的 Settings 节点
/// 是整行，齿轮图标在最右侧）。对宽节点额外给出 bounds 并标注，提醒执行者对照截图，必要时改用坐标。
pub fn render_listing(els: &[Element], screen_w: i32) -> String {
    els.iter()
        .enumerate()
        .map(|(i, e)| {
            let (cx, cy) = e.bounds.center();
            let mut line = format!("[{}] {} ({}) 中心=({cx}, {cy})", i + 1, label(e), if e.clickable { "可点" } else { "只读" });
            let w = e.bounds.right - e.bounds.left;
            if screen_w > 0 && e.clickable && f64::from(w) >= f64::from(screen_w) * WIDE_RATIO {
                let b = e.bounds;
                line.push_str(&format!(" ⚠宽节点 bounds=({},{},{},{})，中心不一定是可点位置", b.left, b.top, b.right, b.bottom));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// 5×7 点阵数字，每行 5 位
const DIGITS: [[u8; 7]; 10] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
    [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
];
const SCALE: i32 = 3; // 每个点阵像素放大成 3×3
const GLYPH_W: i32 = 5 * SCALE;
const GLYPH_H: i32 = 7 * SCALE;
const GAP: i32 = SCALE;

fn put(img: &mut RgbImage, x: i32, y: i32, c: Rgb<u8>) {
    if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
        img.put_pixel(x as u32, y as u32, c);
    }
}

fn draw_number(img: &mut RgbImage, cx: i32, cy: i32, n: usize) {
    let digits: Vec<usize> = n.to_string().bytes().map(|b| usize::from(b - b'0')).collect();
    let total_w = digits.len() as i32 * GLYPH_W + (digits.len() as i32 - 1) * GAP;
    let (x0, y0) = (cx - total_w / 2, cy - GLYPH_H / 2);
    for (k, d) in digits.iter().enumerate() {
        let gx = x0 + k as i32 * (GLYPH_W + GAP);
        for (row, bits) in DIGITS[*d].iter().enumerate() {
            for col in 0..5 {
                if bits & (1 << (4 - col)) != 0 {
                    for dy in 0..SCALE {
                        for dx in 0..SCALE {
                            put(img, gx + col * SCALE + dx, y0 + row as i32 * SCALE + dy, WHITE);
                        }
                    }
                }
            }
        }
    }
}

/// 在每个元素中心画红圆和编号，方便模型把列表和截图对上。中心坐标非正的元素不标。
pub fn annotate(png: &[u8], els: &[Element]) -> Result<RgbImage, image::ImageError> {
    let mut img = image::load_from_memory_with_format(png, ImageFormat::Png)?.to_rgb8();
    for (i, e) in els.iter().enumerate() {
        let (cx, cy) = e.bounds.center();
        if cx <= 0 || cy <= 0 {
            continue;
        }
        for y in -RADIUS..=RADIUS {
            for x in -RADIUS..=RADIUS {
                let d2 = x * x + y * y;
                if d2 <= RADIUS * RADIUS {
                    // 外圈 2px 白边，里面红色
                    let c = if d2 > (RADIUS - 2) * (RADIUS - 2) { WHITE } else { RED };
                    put(&mut img, cx + x, cy + y, c);
                }
            }
        }
        draw_number(&mut img, cx, cy, i + 1);
    }
    Ok(img)
}

pub fn to_png(img: &RgbImage) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img.clone()).write_to(&mut buf, ImageFormat::Png).expect("写内存不会失败");
    buf.into_inner()
}

/// 缩到指定宽度并编码成 JPEG：省 token，也避免超过传输缓冲。
pub fn to_small_jpeg(img: &RgbImage, width: u32, quality: u8) -> Vec<u8> {
    let h = (u64::from(img.height()) * u64::from(width) / u64::from(img.width().max(1))).max(1) as u32;
    let small = image::imageops::resize(img, width, h, FilterType::Triangle);
    let mut buf = Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality)
        .encode_image(&DynamicImage::ImageRgb8(small))
        .expect("写内存不会失败");
    buf.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use device::Bounds;

    fn el(desc: &str, class: &str, clickable: bool, l: i32, t: i32, r: i32, b: i32) -> Element {
        Element {
            desc: desc.into(), text: String::new(), class: class.into(), resource_id: String::new(),
            bounds: Bounds { left: l, top: t, right: r, bottom: b }, clickable, enabled: true, scrollable: false,
        }
    }

    fn white_png(w: u32, h: u32) -> Vec<u8> {
        to_png(&RgbImage::from_pixel(w, h, WHITE))
    }

    #[test]
    fn 名字优先content_desc再退到类名() {
        assert_eq!(label(&el("Add Music", "x.View", true, 0, 0, 1, 1)), "Add Music");
        assert_eq!(label(&el("a\nb", "x.View", true, 0, 0, 1, 1)), "a / b", "换行换成 ' / '");
        assert_eq!(label(&el("", "android.widget.ImageView", true, 0, 0, 1, 1)), "ImageView");
        assert_eq!(label(&el("", "", true, 0, 0, 1, 1)), "元素");
    }

    #[test]
    fn 编号列表格式和宽节点提示() {
        let els = [el("Add(1)", "v", true, 22, 1487, 698, 1577), el("Title", "v", false, 100, 100, 200, 160), el("Settings", "v", true, 0, 300, 720, 380)];
        let l = render_listing(&els, 720);
        let lines: Vec<&str> = l.lines().collect();
        assert_eq!(lines[0], "[1] Add(1) (可点) 中心=(360, 1532) ⚠宽节点 bounds=(22,1487,698,1577)，中心不一定是可点位置");
        assert_eq!(lines[1], "[2] Title (只读) 中心=(150, 130)", "不可点、不宽的元素没有提示");
        assert!(lines[2].contains("⚠宽节点"));
        assert!(!render_listing(&els, 0).contains("宽节点"), "取不到屏幕宽度时不标宽节点");
    }

    #[test]
    fn 只读的宽节点不标注() {
        let l = render_listing(&[el("Banner", "v", false, 0, 0, 720, 100)], 720);
        assert!(!l.contains("宽节点"), "宽节点提示只针对可点元素：{l}");
    }

    #[test]
    fn 标注在元素中心画红圆() {
        let img = annotate(&white_png(300, 300), &[el("A", "v", true, 100, 100, 200, 200)]).unwrap();
        // 圆心 (150,150)；偏移 18px 在圆内、数字（半宽约 8px）之外
        assert_eq!(*img.get_pixel(168, 150), RED);
        assert_eq!(*img.get_pixel(150 + RADIUS as u32 + 8, 150), WHITE, "圆外保持原图");
        assert_eq!(*img.get_pixel(150 + RADIUS as u32 - 1, 150), WHITE, "最外圈是白边");
    }

    #[test]
    fn 编号数字真的画出来了() {
        let img = annotate(&white_png(300, 300), &[el("A", "v", true, 100, 100, 200, 200)]).unwrap();
        let white_in_core = (-10..=10).flat_map(|y| (-10..=10).map(move |x| (x, y))).filter(|(x, y)| *img.get_pixel((150 + x) as u32, (150 + y) as u32) == WHITE).count();
        assert!(white_in_core > 20, "圆心附近应有白色数字笔画，实际 {white_in_core} 个白点");
    }

    #[test]
    fn 多位数编号和越界不panic() {
        let els: Vec<Element> = (0..120).map(|i| el("x", "v", true, i * 2, 5, i * 2 + 4, 9)).collect();
        annotate(&white_png(300, 300), &els).expect("120 个元素（三位数编号）、圆压到画布边缘也不能 panic");
        // 中心非正的元素（0,0 附近）直接跳过
        annotate(&white_png(50, 50), &[el("z", "v", true, 0, 0, 0, 0)]).unwrap();
    }

    #[test]
    fn 非png数据返回错误而不是panic() {
        assert!(annotate(b"not a png", &[]).is_err());
    }

    #[test]
    fn 缩小成jpeg() {
        let img = RgbImage::from_pixel(720, 1600, WHITE);
        let j = to_small_jpeg(&img, 540, 70);
        assert_eq!(&j[..2], &[0xFF, 0xD8], "应是 JPEG");
        let back = image::load_from_memory(&j).unwrap();
        assert_eq!((back.width(), back.height()), (540, 1200), "按宽度等比缩小");
    }
}
