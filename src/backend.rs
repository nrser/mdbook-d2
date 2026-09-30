use std::{
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::bail;
use mdbook_preprocessor::{book::SectionNumber, PreprocessorContext};
use pulldown_cmark::{Event, Tag, TagEnd};
use semver::Version;

use crate::config::Config;

/// CSS class applied to rendered D2 diagram HTML so it can be targeted with
/// user stylesheets.
const DIAGRAM_CLASS: &str = "mdbook-d2";

/// We're using `r#"..."#` for HTML strings so we don't have to escape double-quotes around
/// attribute values, which I guess doesn't recognize `\n` newlines, so interpolate this instead.
const N: &str = "\n";

/// First `d2` release that accepts `--no-xml-tag`.
///
/// <https://github.com/terrastruct/d2/releases/tag/v0.6.9>
const NO_XML_TAG_MIN: Version = Version::new(0, 6, 9);

/// Represents the backend for processing D2 diagrams
pub struct Backend {
    /// Configuration from user's `book.toml`
    config: Config,
    /// Absolute path to the source directory of the book
    source_dir: PathBuf,
    /// `d2` accepts `--no-xml-tag` (0.6.9 or newer).
    supports_no_xml_tag: bool,
}

/// Context for rendering a specific diagram
#[derive(Debug, Clone, Copy)]
pub struct RenderContext<'a> {
    /// Relative path to the current chapter file
    path: &'a Path,
    /// Name of the current chapter
    chapter: &'a str,
    /// Section number of the current chapter
    section: Option<&'a SectionNumber>,
    /// Index of the current diagram within the chapter
    diagram_index: usize,
}

impl<'a> RenderContext<'a> {
    /// Creates a new [`RenderContext`]
    pub const fn new(
        path: &'a Path,
        chapter: &'a str,
        section: Option<&'a SectionNumber>,
        diagram_index: usize,
    ) -> Self {
        Self {
            path,
            chapter,
            section,
            diagram_index,
        }
    }
}

/// Generates a filename for a diagram based on its context
///
/// Returns a relative path for the diagram file
fn filename(ctx: &RenderContext) -> String {
    format!(
        "{}{}.svg",
        ctx.section.cloned().unwrap_or_default(),
        ctx.diagram_index
    )
}

impl Backend {
    /// Creates a new Backend instance
    ///
    /// # Arguments
    /// * `config` - Configuration for the D2 preprocessor
    /// * `source_dir` - Absolute path to the book's source directory
    pub fn new(config: Config, source_dir: PathBuf) -> Self {
        let supports_no_xml_tag =
            d2_version(&config.path).is_some_and(|version| version >= NO_XML_TAG_MIN);
        Self {
            config,
            source_dir,
            supports_no_xml_tag,
        }
    }

    /// Creates a Backend instance from a [`PreprocessorContext`]
    ///
    /// # Arguments
    /// * `ctx` - The preprocessor context
    pub fn from_context(ctx: &PreprocessorContext) -> Self {
        let config: Config = ctx
            .config
            .get("preprocessor.d2")
            .expect("Unable to deserialize d2 preprocessor config")
            .expect("d2 preprocessor config not found");
        let source_dir = ctx.root.join(&ctx.config.book.src);

        Self::new(config, source_dir)
    }

    /// Constructs the absolute file path for a diagram
    ///
    /// # Arguments
    /// * `ctx` - The render context for the diagram
    fn filepath(&self, ctx: &RenderContext) -> PathBuf {
        let filepath = Path::new(&self.source_dir).join(self.relative_file_path(ctx));
        filepath
    }

    /// Constructs the relative file path for a diagram
    ///
    /// # Arguments
    /// * `ctx` - The render context for the diagram
    fn relative_file_path(&self, ctx: &RenderContext) -> PathBuf {
        let filename = filename(ctx);
        self.config.output_dir.join(filename)
    }

    /// Renders a D2 diagram and returns the appropriate markdown events
    ///
    /// # Arguments
    /// * `ctx` - The render context for the diagram
    /// * `content` - The D2 diagram content
    pub fn render(
        &self,
        ctx: &RenderContext,
        content: &str,
    ) -> anyhow::Result<Vec<Event<'static>>> {
        if self.config.inline {
            self.render_inline(ctx, content)
        } else {
            self.render_embedded(ctx, content)
        }
    }

    /// Render the diagram SVG source directly into the markdown source inside an [HTML block].
    ///
    /// Used when [`mdbook_d2::config::Config::inline`] is `true` (default).
    ///
    /// SVG source is wrapped in a `<pre>` to tolerate any blank lines, with `class="mdbook-d2"` for
    /// style targeting.
    ///
    /// [HTML block]: https://spec.commonmark.org/0.31.2/#html-blocks
    fn render_inline(
        &self,
        ctx: &RenderContext,
        content: &str,
    ) -> anyhow::Result<Vec<Event<'static>>> {
        let args = self.basic_args();
        let diagram = self.run_process(ctx, content, args)?;

        // We can only emit markdown — because [preprocessors] are [backend]-agnostic — but markdown
        // includes a raw [HTML block] we can use: `Tag::HtmlBlock` around a `Event:Html` for each
        // line.
        //
        // [preprocessor]: https://rust-lang.github.io/mdBook/for_developers/preprocessors.html
        // [backend]: https://rust-lang.github.io/mdBook/for_developers/backends.html
        // [HTML block]: https://spec.commonmark.org/0.31.2/#html-blocks
        Ok(vec![
            Event::Start(Tag::HtmlBlock),
            // Nested inside a `<pre>` to avoid prematurely breaking out of the [HTML block] on any
            // blank lines that may be in the diagram SVG source.
            //
            // NOTE Each `Event::Html` needs to be `\n`-terminated.
            Event::Html(format!("<pre class=\"{DIAGRAM_CLASS}\">{diagram}</pre>\n").into()),
            Event::End(TagEnd::HtmlBlock),
        ])
    }

    /// Render the diagram SVG to a file and include it via an `<img>` tag.
    ///
    /// Used when [`mdbook_d2::config::Config::inline`] is `false`.
    ///
    /// The image sits in a `<div class="mdbook-d2">` for styling. When
    /// [`mdbook_d2::config::Config::link_img`] is set, the `<img>` is wrapped in an `<a>` that
    /// opens the SVG in a new tab. Emitted into the markdown source as a raw [HTML block].
    ///
    /// [HTML block]: https://spec.commonmark.org/0.31.2/#html-blocks
    fn render_embedded(
        &self,
        ctx: &RenderContext,
        content: &str,
    ) -> anyhow::Result<Vec<Event<'static>>> {
        fs::create_dir_all(Path::new(&self.source_dir).join(&self.config.output_dir)).unwrap();
        let mut args = self.basic_args();
        let filepath = self.filepath(ctx);
        args.push(filepath.as_os_str());

        self.run_process(ctx, content, args)?;

        let depth = ctx.path.ancestors().count() - 2;
        let rel_path: PathBuf = std::iter::repeat_n(Path::new(".."), depth)
            .collect::<PathBuf>()
            .join(self.relative_file_path(ctx));
        let src = rel_path.to_string_lossy().replace('\\', "/");

        // Raw HTML block, one `Event::Html` for each line to emit. See comment in
        // [`render_inline`].
        let mut events = vec![
            Event::Start(Tag::HtmlBlock),
            // NOTE Each `Event::Html` needs to be `\n`-terminated.
            // NOTE No blank lines! See https://spec.commonmark.org/0.31.2/#html-blocks
            Event::Html(format!(r#"<div class="{DIAGRAM_CLASS}">{N}"#).into()),
        ];
        if self.config.link_img {
            events.extend([
                Event::Html(format!(r#"    <a href="{src}" target="_blank">{N}"#).into()),
                Event::Html(format!(r#"        <img src="{src}" alt="" />{N}"#).into()),
                Event::Html("    </a>\n".into()),
            ]);
        } else {
            events.push(Event::Html(
                format!(r#"    <img src="{src}" alt="" />{N}"#).into(),
            ));
        }
        events.extend([
            Event::Html("</div>\n".into()),
            Event::End(TagEnd::HtmlBlock),
        ]);
        Ok(events)
    }

    fn basic_args(&self) -> Vec<&OsStr> {
        let mut args = vec![];

        if let Some(fonts) = &self.config.fonts {
            args.extend([
                OsStr::new("--font-regular"),
                fonts.regular.as_os_str(),
                OsStr::new("--font-italic"),
                fonts.italic.as_os_str(),
                OsStr::new("--font-bold"),
                fonts.bold.as_os_str(),
            ]);
        }
        if let Some(layout) = &self.config.layout {
            args.extend([OsStr::new("--layout"), layout.as_ref()]);
        }
        if let Some(theme_id) = &self.config.theme_id {
            args.extend([OsStr::new("--theme"), theme_id.as_ref()]);
        }
        if let Some(dark_theme_id) = &self.config.dark_theme_id {
            args.extend([OsStr::new("--dark-theme"), dark_theme_id.as_ref()]);
        }
        // The XML declaration is invalid in HTML, so drop it only when the SVG is inlined.
        if self.config.inline && self.supports_no_xml_tag {
            args.push(OsStr::new("--no-xml-tag"));
        }
        args.push(OsStr::new("-"));
        args
    }

    /// Runs the D2 process to generate a diagram
    ///
    /// # Arguments
    /// * `ctx` - The render context for the diagram
    /// * `content` - The D2 diagram content
    /// * `args` - Additional arguments for the D2 process
    fn run_process<I, S>(
        &self,
        ctx: &RenderContext,
        content: &str,
        args: I,
    ) -> anyhow::Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let child = Command::new(&self.config.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(args)
            .spawn()?;

        child
            .stdin
            .as_ref()
            .unwrap()
            .write_all(content.as_bytes())?;

        let output = child.wait_with_output()?;
        if output.status.success() {
            let diagram = String::from_utf8_lossy(&output.stdout).to_string();
            Ok(diagram)
        } else {
            let src =
                format!("\n{}", String::from_utf8_lossy(&output.stderr)).replace('\n', "\n  ");
            let msg = format!(
                "failed to compile D2 diagram ({}, #{}):{src}",
                ctx.chapter, ctx.diagram_index
            );
            bail!(msg)
        }
    }
}

/// Read `d2 --version`.
///
/// # Arguments
/// * `path` - Program name or location. From [`Config::path`], which defaults to `"d2"`.
fn d2_version(path: &Path) -> Option<Version> {
    let output = match Command::new(path).arg("--version").output() {
        Ok(output) => output,
        Err(err) => {
            eprintln!(
                "warning: failed to run `{} --version`: {err}",
                path.display()
            );
            return None;
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            eprintln!(
                "warning: `{} --version` failed ({})",
                path.display(),
                output.status
            );
        } else {
            eprintln!(
                "warning: `{} --version` failed ({}): {stderr}",
                path.display(),
                output.status
            );
        }
        return None;
    }
    let raw = if output.stdout.is_empty() {
        output.stderr
    } else {
        output.stdout
    };
    let text = String::from_utf8_lossy(&raw);
    parse_d2_version(&text)
        .inspect_err(|err| {
            eprintln!(
                "warning: could not parse `{} --version` output {text:?}: {err}",
                path.display()
            );
        })
        .ok()
}

/// Parse `d2 --version` output, accepting an optional leading `v`.
fn parse_d2_version(output: &str) -> anyhow::Result<Version> {
    let Some(token) = output.split_whitespace().next() else {
        bail!("empty version output");
    };
    let token = token.strip_prefix('v').unwrap_or(token);
    Ok(Version::parse(token)?)
}

#[cfg(test)]
mod tests {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    use pulldown_cmark_to_cmark::cmark_with_options;

    use super::{parse_d2_version, DIAGRAM_CLASS, N, NO_XML_TAG_MIN};

    fn round_trip_html(events: Vec<Event<'_>>) -> String {
        let mut markdown = String::new();
        cmark_with_options(
            events.into_iter(),
            &mut markdown,
            pulldown_cmark_to_cmark::Options::default(),
        )
        .unwrap();

        let mut html = String::new();
        pulldown_cmark::html::push_html(&mut html, Parser::new_ext(&markdown, Options::all()));
        html
    }

    #[test]
    fn parses_d2_version_output() {
        let current = parse_d2_version("v0.7.1\n").expect("v0.7.1");
        assert!(current >= NO_XML_TAG_MIN);

        let boundary = parse_d2_version("0.6.9").expect("0.6.9");
        assert!(boundary >= NO_XML_TAG_MIN);

        let older = parse_d2_version("v0.6.8\n").expect("v0.6.8");
        assert!(older < NO_XML_TAG_MIN);

        assert!(parse_d2_version("not a version").is_err());
        assert!(parse_d2_version("").is_err());
    }

    #[test]
    fn html_block_is_separated_from_neighboring_paragraphs() {
        let mut events = vec![
            Event::Start(Tag::Paragraph),
            Event::Text("Hello.".into()),
            Event::End(TagEnd::Paragraph),
        ];
        let src = "d2/1.1.svg";
        events.extend([
            Event::Start(Tag::HtmlBlock),
            Event::Html(format!(r#"<div class="{DIAGRAM_CLASS}">{N}"#).into()),
            Event::Html(format!(r#"    <a href="{src}" target="_blank">{N}"#).into()),
            Event::Html(format!(r#"        <img src="{src}" alt="" />{N}"#).into()),
            Event::Html("    </a>\n".into()),
            Event::Html("</div>\n".into()),
            Event::End(TagEnd::HtmlBlock),
        ]);
        events.extend([
            Event::Start(Tag::Paragraph),
            Event::Text("Goodbye.".into()),
            Event::End(TagEnd::Paragraph),
        ]);

        let html = round_trip_html(events);

        assert!(
            html.contains("<p>Hello.</p>"),
            "previous paragraph should close before the diagram, got: {html}"
        );
        assert!(
            html.contains("<p>Goodbye.</p>"),
            "following paragraph should start after the diagram, got: {html}"
        );
        assert!(
            !html.contains("<p></p>"),
            "block HTML should not leave an empty paragraph, got: {html}"
        );
        assert!(
            html.contains(r#"<div class="mdbook-d2">"#)
                && html.contains(r#"href="d2/1.1.svg""#)
                && html.contains(r#"src="d2/1.1.svg""#),
            "diagram should render as a classed linked image, got: {html}"
        );
    }
}
