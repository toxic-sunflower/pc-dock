//! Элементы экрана из ответа `UiDump` (uiautomator XML) и поиск по ним для сценариев (`p:find`, `p:tap_on`).
//! Разбор упрощённый: берём только теги `<node …>` и их атрибуты — этого хватает для дампа uiautomator.

/// Один элемент экрана.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    pub text: String,
    /// `resource-id` целиком (`com.app:id/login`).
    pub id: String,
    pub desc: String,
    pub class: String,
    pub package: String,
    pub clickable: bool,
    pub enabled: bool,
    /// Левый верхний и правый нижний угол в пикселях экрана.
    pub bounds: (i32, i32, i32, i32),
}

impl Node {
    pub fn center(&self) -> (f32, f32) {
        let (x1, y1, x2, y2) = self.bounds;
        ((x1 + x2) as f32 / 2.0, (y1 + y2) as f32 / 2.0)
    }
}

/// Что ищем. Пустое поле не участвует; все заданные должны совпасть.
#[derive(Clone, Debug, Default)]
pub struct Selector {
    /// Текст целиком (без учёта регистра и пробелов по краям).
    pub text: Option<String>,
    /// Текст содержит подстроку (без учёта регистра).
    pub text_contains: Option<String>,
    /// `resource-id`: целиком или только имя после `:id/`.
    pub id: Option<String>,
    /// `content-desc` целиком (без учёта регистра).
    pub desc: Option<String>,
    /// Класс целиком или его последняя часть (`Button`).
    pub class: Option<String>,
    /// Номер совпадения с нуля (по порядку в дереве).
    pub index: usize,
}

impl Selector {
    pub fn is_empty(&self) -> bool {
        self.text.is_none()
            && self.text_contains.is_none()
            && self.id.is_none()
            && self.desc.is_none()
            && self.class.is_none()
    }

    pub fn matches(&self, n: &Node) -> bool {
        let eq = |a: &str, b: &str| a.trim().to_lowercase() == b.trim().to_lowercase();
        self.text.as_deref().is_none_or(|t| eq(&n.text, t))
            && self.text_contains.as_deref().is_none_or(|t| n.text.to_lowercase().contains(&t.to_lowercase()))
            && self.id.as_deref().is_none_or(|i| n.id == i || n.id.rsplit(":id/").next() == Some(i))
            && self.desc.as_deref().is_none_or(|d| eq(&n.desc, d))
            && self.class.as_deref().is_none_or(|c| n.class == c || n.class.rsplit('.').next() == Some(c))
    }

    /// Описание для журнала: `text="Войти" id=login`.
    pub fn describe(&self) -> String {
        let mut v = Vec::new();
        for (k, val) in [
            ("text", &self.text),
            ("text_contains", &self.text_contains),
            ("id", &self.id),
            ("desc", &self.desc),
            ("class", &self.class),
        ] {
            if let Some(x) = val {
                v.push(format!("{k}={x:?}"));
            }
        }
        if self.index > 0 {
            v.push(format!("index={}", self.index));
        }
        v.join(" ")
    }
}

/// Все элементы дампа по порядку.
pub fn parse(xml: &str) -> Vec<Node> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<node") {
        rest = &rest[start + 5..];
        let Some(end) = tag_end(rest) else { break };
        let attrs = &rest[..end];
        rest = &rest[end..];
        let get = |name: &str| attr(attrs, name).unwrap_or_default();
        out.push(Node {
            text: get("text"),
            id: get("resource-id"),
            desc: get("content-desc"),
            class: get("class"),
            package: get("package"),
            clickable: get("clickable") == "true",
            enabled: get("enabled") != "false",
            bounds: bounds(&get("bounds")).unwrap_or_default(),
        });
    }
    out
}

/// Конец открывающего тега (`>` вне кавычек).
fn tag_end(s: &str) -> Option<usize> {
    let mut quoted = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '>' if !quoted => return Some(i),
            _ => {}
        }
    }
    None
}

fn attr(attrs: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let from = attrs.find(&key)? + key.len();
    let len = attrs[from..].find('"')?;
    Some(unescape(&attrs[from..from + len]))
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#10;", "\n")
        .replace("&amp;", "&")
}

/// `[x1,y1][x2,y2]`.
fn bounds(s: &str) -> Option<(i32, i32, i32, i32)> {
    let nums: Vec<i32> = s
        .split(|c: char| !(c.is_ascii_digit() || c == '-'))
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect();
    match nums[..] {
        [x1, y1, x2, y2] => Some((x1, y1, x2, y2)),
        _ => None,
    }
}

/// Размер экрана по дампу: правый нижний угол самого большого элемента (первый узел — окно целиком).
pub fn screen_size(nodes: &[Node]) -> Option<(i32, i32)> {
    let w = nodes.iter().map(|n| n.bounds.2).max()?;
    let h = nodes.iter().map(|n| n.bounds.3).max()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// Найти элемент по селектору: `index`-е совпадение.
pub fn find<'a>(nodes: &'a [Node], sel: &Selector) -> Option<&'a Node> {
    nodes.iter().filter(|n| sel.matches(n)).nth(sel.index)
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.app" content-desc="" clickable="false" enabled="true" bounds="[0,0][1080,2400]"><node index="0" text="Войти" resource-id="com.app:id/login" class="android.widget.Button" package="com.app" content-desc="" clickable="true" enabled="true" bounds="[100,1000][500,1200]" /><node index="1" text="A &amp; B &gt; C" resource-id="" class="android.widget.TextView" package="com.app" content-desc="Меню" clickable="false" enabled="true" bounds="[0,0][200,100]" /></node></hierarchy>"#;

    #[test]
    fn parses_nodes_and_finds_by_text_id_desc_class() {
        let nodes = parse(XML);
        assert_eq!(nodes.len(), 3);
        assert_eq!(screen_size(&nodes), Some((1080, 2400)));
        let by_text = find(&nodes, &Selector { text: Some(" войти ".into()), ..Default::default() }).unwrap();
        assert_eq!(by_text.id, "com.app:id/login");
        assert_eq!(by_text.center(), (300.0, 1100.0));
        assert!(by_text.clickable);
        assert!(find(&nodes, &Selector { id: Some("login".into()), ..Default::default() }).is_some());
        assert!(find(&nodes, &Selector { class: Some("Button".into()), ..Default::default() }).is_some());
        let menu = find(&nodes, &Selector { desc: Some("меню".into()), ..Default::default() }).unwrap();
        assert_eq!(menu.text, "A & B > C");
        assert!(find(&nodes, &Selector { text_contains: Some("b >".into()), ..Default::default() }).is_some());
        assert!(find(&nodes, &Selector { text: Some("Нет".into()), ..Default::default() }).is_none());
        assert!(find(&nodes, &Selector { class: Some("Button".into()), index: 1, ..Default::default() })
            .is_none());
    }
}
