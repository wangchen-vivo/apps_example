// Copyright (c) 2026 vivo Mobile Communication Co., Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Bounded pagination for Theme.font-base (20 px) in the SD card text viewer.
//! The viewer sits between AppPage's 60 px title bar and 56 px bottom nav, with
//! its own 50 px header (filename + page number), leaving 312 px of text height.
//! Text width is 448 px (480 screen - 16 px margins); four pixels are reserved
//! for glyph overhang.

mod metrics {
    include!("../resources/text-metrics.rs");
}

const FONT_SIZE: u32 = 20;
const LINE_WIDTH: u32 = 444;
const PAGE_LINES: usize = 12;

fn advance(character: char) -> u32 {
    let units = metrics::ADVANCES
        .binary_search_by_key(&(character as u32), |&(code, _)| code)
        .map(|index| metrics::ADVANCES[index].1)
        .unwrap_or(metrics::UNITS_PER_EM);
    (units * FONT_SIZE).div_ceil(metrics::UNITS_PER_EM)
}

fn finish_line(
    page: &mut String,
    line: &mut usize,
    page_count: &mut usize,
    emit: &mut impl FnMut(&str),
) {
    if *line + 1 == PAGE_LINES {
        emit(page);
        *page_count += 1;
        page.clear();
        *line = 0;
    } else {
        page.push('\n');
        *line += 1;
    }
}

fn emit_pages(contents: &str, mut emit: impl FnMut(&str)) {
    let mut page = String::new();
    let mut page_count = 0;
    let mut line = 0;
    let mut width = 0;
    let mut columns = 0;
    let mut ended_with_newline = false;
    for character in contents.trim_start_matches('\u{feff}').chars() {
        if character == '\r' {
            continue;
        }
        if character == '\n' {
            finish_line(&mut page, &mut line, &mut page_count, &mut emit);
            width = 0;
            columns = 0;
            ended_with_newline = true;
            continue;
        }
        ended_with_newline = false;
        let (character, repeats) = match character {
            '\t' => (' ', 4 - columns % 4),
            character if character.is_control() => ('\u{fffd}', 1),
            character => (character, 1),
        };
        for _ in 0..repeats {
            let next_width = advance(character);
            if width != 0 && width + next_width > LINE_WIDTH {
                finish_line(&mut page, &mut line, &mut page_count, &mut emit);
                width = 0;
                columns = 0;
            }
            page.push(character);
            width += next_width;
            columns += 1;
        }
    }
    if !page.is_empty() {
        if ended_with_newline {
            page.pop();
        }
        emit(&page);
        page_count += 1;
    }
    if page_count == 0 {
        emit("(empty file)");
    }
}

pub fn paginate_text(contents: &str) -> Vec<String> {
    let mut pages = Vec::new();
    emit_pages(contents, |page| pages.push(page.to_owned()));
    pages
}

/// Retains only the bounded source text. Pages are generated on demand by
/// scanning at most 8 KiB, avoiding storage proportional to the page count.
#[derive(Default)]
pub struct OnDemandTextPages {
    contents: String,
    page_count: usize,
}

impl OnDemandTextPages {
    pub fn new(contents: String) -> Self {
        let mut page_count = 0;
        emit_pages(&contents, |_| page_count += 1);
        Self {
            contents,
            page_count,
        }
    }

    pub fn get(&self, index: usize) -> Option<String> {
        if index >= self.page_count {
            return None;
        }
        let mut current = 0;
        let mut result = None;
        emit_pages(&self.contents, |page| {
            if current == index {
                result = Some(page.to_owned());
            }
            current += 1;
        });
        result
    }

    pub fn len(&self) -> usize {
        self.page_count
    }

    pub fn is_empty(&self) -> bool {
        self.page_count == 0
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_bounds(pages: &[String]) {
        for page in pages {
            assert!(page.split('\n').count() <= PAGE_LINES);
            for line in page.split('\n') {
                assert!(line.chars().map(advance).sum::<u32>() <= LINE_WIDTH);
            }
        }
    }

    #[test]
    fn mixed_text_survives_pagination() {
        let text = "蓝河内核 Rust WWWW iii 480×480！".repeat(120);
        let pages = paginate_text(&text);
        check_bounds(&pages);
        assert!(pages.len() > 1);
        assert_eq!(pages.concat().replace('\n', ""), text);
    }

    #[test]
    fn newlines_tabs_and_controls() {
        assert_eq!(paginate_text("\u{feff}A\tB\r\nC\0"), ["A   B\nC�"]);
        assert_eq!(paginate_text(""), ["(empty file)"]);
        assert_eq!(paginate_text("\r\n"), [""]);
    }

    #[test]
    fn exact_page_boundary() {
        let pages = paginate_text(&"one\n".repeat(PAGE_LINES));
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].split('\n').count(), PAGE_LINES);
        let pages = paginate_text(&format!("{}last", "one\n".repeat(PAGE_LINES)));
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[1], "last");
    }

    #[test]
    fn newline_heavy_file_is_bounded() {
        let pages = paginate_text(&"\n".repeat(8 * 1024));
        check_bounds(&pages);
        assert_eq!(pages.len(), (8usize * 1024).div_ceil(PAGE_LINES));
        assert!(pages.iter().map(String::capacity).sum::<usize>() <= 16 * 1024);

        let on_demand = OnDemandTextPages::new("\n".repeat(8 * 1024));
        assert_eq!(on_demand.len(), pages.len());
        for (index, page) in pages.iter().enumerate() {
            assert_eq!(on_demand.get(index).as_deref(), Some(page.as_str()));
        }
        assert!(on_demand.contents.capacity() <= 8 * 1024);
    }

    #[test]
    fn samples_fit_and_have_embedded_glyphs() {
        for contents in [
            include_str!("../assets/website/01-introduction.txt"),
            include_str!("../assets/website/02-features.txt"),
            include_str!("../assets/website/03-capabilities.txt"),
            include_str!("../assets/website/04-memory.txt"),
            include_str!("../assets/website/05-filesystem.txt"),
            include_str!("../assets/website/06-drivers.txt"),
            include_str!("../assets/website/README.txt"),
        ] {
            assert!(contents.len() <= 8 * 1024);
            check_bounds(&paginate_text(contents));
            for character in contents.chars().filter(|character| !character.is_control()) {
                assert!(
                    metrics::ADVANCES
                        .binary_search_by_key(&(character as u32), |&(code, _)| code)
                        .is_ok(),
                    "missing glyph: {character}"
                );
            }
        }
    }
}
