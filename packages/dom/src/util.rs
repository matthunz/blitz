use std::{
    io::{Cursor, Read},
    sync::{Arc, OnceLock},
    time::Instant,
};

use crate::node::{Node, NodeData};
use image::DynamicImage;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:60.0) Gecko/20100101 Firefox/81.0";
const FILE_SIZE_LIMIT: u64 = 1_000_000_000; // 1GB

static FONT_DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();

pub(crate) enum FetchErr {
    UrlParse(url::ParseError),
    Ureq(Box<ureq::Error>),
    FileIo(std::io::Error),
}
impl From<url::ParseError> for FetchErr {
    fn from(value: url::ParseError) -> Self {
        Self::UrlParse(value)
    }
}
impl From<Box<ureq::Error>> for FetchErr {
    fn from(value: Box<ureq::Error>) -> Self {
        Self::Ureq(value)
    }
}
impl From<std::io::Error> for FetchErr {
    fn from(value: std::io::Error) -> Self {
        Self::FileIo(value)
    }
}

pub(crate) fn fetch_blob(url: &str) -> Result<Vec<u8>, FetchErr> {
    let start = Instant::now();

    // Handle data URIs
    if url.starts_with("data:") {
        let data_url = data_url::DataUrl::process(url).unwrap();
        let decoded = data_url.decode_to_vec().expect("Invalid data url");
        return Ok(decoded.0);
    }

    // Handle file:// URLs
    let parsed_url = Url::parse(url)?;
    if parsed_url.scheme() == "file" {
        let file_content = std::fs::read(parsed_url.path())?;
        return Ok(file_content);
    }

    let resp = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(Box::new)?;

    let len: usize = resp
        .header("Content-Length")
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut bytes: Vec<u8> = Vec::with_capacity(len);

    resp.into_reader()
        .take(FILE_SIZE_LIMIT)
        .read_to_end(&mut bytes)
        .unwrap();

    let time = (Instant::now() - start).as_millis();
    println!("Fetched {} in {}ms", url, time);

    Ok(bytes)
}

pub(crate) fn fetch_string(url: &str) -> Result<String, FetchErr> {
    fetch_blob(url).map(|vec| String::from_utf8(vec).expect("Invalid UTF8"))
}

// pub(crate) fn fetch_buffered_stream(
//     url: &str,
// ) -> Result<impl BufRead + Read + Send + Sync, ureq::Error> {
//     let resp = ureq::get(url).set("User-Agent", USER_AGENT).call()?;
//     Ok(BufReader::new(resp.into_reader().take(FILE_SIZE_LIMIT)))
// }

pub(crate) enum ImageOrSvg {
    Image(DynamicImage),
    Svg(usvg::Tree),
}

#[allow(unused)]
pub(crate) enum ImageFetchErr {
    UrlParse(url::ParseError),
    Ureq(Box<ureq::Error>),
    FileIo(std::io::Error),
    ImageParse(image::error::ImageError),
    SvgParse(usvg::Error),
}

impl From<FetchErr> for ImageFetchErr {
    fn from(value: FetchErr) -> Self {
        match value {
            FetchErr::UrlParse(err) => Self::UrlParse(err),
            FetchErr::Ureq(err) => Self::Ureq(err),
            FetchErr::FileIo(err) => Self::FileIo(err),
        }
    }
}
impl From<image::error::ImageError> for ImageFetchErr {
    fn from(value: image::error::ImageError) -> Self {
        Self::ImageParse(value)
    }
}
impl From<usvg::Error> for ImageFetchErr {
    fn from(value: usvg::Error) -> Self {
        Self::SvgParse(value)
    }
}

pub(crate) fn fetch_image(url: &str) -> Result<ImageOrSvg, ImageFetchErr> {
    let blob = crate::util::fetch_blob(url)?;

    // Try parse image
    if let Ok(image) = image::ImageReader::new(Cursor::new(&blob))
        .with_guessed_format()
        .expect("IO errors impossible with Cursor")
        .decode()
    {
        return Ok(ImageOrSvg::Image(image));
    };

    // Try parse SVG

    // TODO: Use fontique
    let fontdb = FONT_DB.get_or_init(|| {
        let mut fontdb = usvg::fontdb::Database::new();
        fontdb.load_system_fonts();
        Arc::new(fontdb)
    });

    let options = usvg::Options {
        fontdb: fontdb.clone(),
        ..Default::default()
    };

    let tree = usvg::Tree::from_data(&blob, &options)?;
    Ok(ImageOrSvg::Svg(tree))
}

// Debug print an RcDom
pub fn walk_tree(indent: usize, node: &Node) {
    // Skip all-whitespace text nodes entirely
    if let NodeData::Text(data) = &node.raw_dom_data {
        if data.content.chars().all(|c| c.is_ascii_whitespace()) {
            return;
        }
    }

    print!("{}", " ".repeat(indent));
    match &node.raw_dom_data {
        NodeData::Document => println!("#Document"),

        NodeData::Text(data) => {
            if data.content.chars().all(|c| c.is_ascii_whitespace()) {
                println!("#text: <whitespace>");
            } else {
                let content = data.content.trim();
                if content.len() > 10 {
                    println!(
                        "#text: {}...",
                        content
                            .split_at(content.char_indices().take(10).last().unwrap().0)
                            .0
                            .escape_default()
                    )
                } else {
                    println!("#text: {}", data.content.trim().escape_default())
                }
            }
        }

        NodeData::Comment => println!("<!-- COMMENT -->"),

        NodeData::AnonymousBlock(_) => println!("AnonymousBlock"),

        NodeData::Element(data) => {
            print!("<{}", data.name.local);
            for attr in data.attrs.iter() {
                print!(" {}=\"{}\"", attr.name.local, attr.value);
            }
            if !node.children.is_empty() {
                println!(">");
            } else {
                println!("/>");
            }
        } // NodeData::Doctype {
          //     ref name,
          //     ref public_id,
          //     ref system_id,
          // } => println!("<!DOCTYPE {} \"{}\" \"{}\">", name, public_id, system_id),
          // NodeData::ProcessingInstruction { .. } => unreachable!(),
    }

    if !node.children.is_empty() {
        for child_id in node.children.iter() {
            walk_tree(indent + 2, node.with(*child_id));
        }

        if let NodeData::Element(data) = &node.raw_dom_data {
            println!("{}</{}>", " ".repeat(indent), data.name.local);
        }
    }
}

use peniko::Color as PenikoColor;
use style::color::AbsoluteColor;
use url::Url;

pub trait ToPenikoColor {
    fn as_peniko(&self) -> PenikoColor;
}
impl ToPenikoColor for AbsoluteColor {
    fn as_peniko(&self) -> PenikoColor {
        let [r, g, b, a] = self
            .to_color_space(style::color::ColorSpace::Srgb)
            .raw_components()
            .map(|f| (f * 255.0) as u8);
        PenikoColor { r, g, b, a }
    }
}
