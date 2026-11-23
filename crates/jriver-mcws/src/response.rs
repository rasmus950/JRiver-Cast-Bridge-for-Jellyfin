//! Parsing of MCWS XML responses:
//! `<Response Status="OK"><Item Name="State">2</Item>...</Response>`.

use crate::{Function, McwsError};

/// The `Item` name/value pairs of a successful response, in document order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Items(Vec<(String, String)>);

impl Items {
  pub(crate) fn get(&self, name: &str) -> Option<&str> {
    self
      .0
      .iter()
      .find(|(key, _)| key == name)
      .map(|(_, value)| value.as_str())
  }

  #[cfg(test)]
  pub(crate) fn len(&self) -> usize {
    self.0.len()
  }
}

/// Parse a response body. `Status="OK"` yields the items; any other status
/// becomes [`McwsError::Failure`] with MC's message when it sent one.
pub(crate) fn parse(function: Function, body: &str) -> Result<Items, McwsError> {
  let doc = roxmltree::Document::parse(body.trim_start_matches('\u{feff}')).map_err(|err| {
    McwsError::Parse {
      function,
      reason: err.to_string(),
    }
  })?;
  let root = doc.root_element();
  if root.tag_name().name() != "Response" {
    return Err(McwsError::Parse {
      function,
      reason: format!(
        "root element is <{}>, not <Response>",
        root.tag_name().name()
      ),
    });
  }

  let items = Items(
    root
      .children()
      .filter(|node| node.is_element() && node.tag_name().name() == "Item")
      .filter_map(|node| {
        let name = node.attribute("Name")?;
        Some((name.to_owned(), node.text().unwrap_or_default().to_owned()))
      })
      .collect(),
  );

  match root.attribute("Status") {
    Some("OK") => Ok(items),
    status => Err(McwsError::Failure {
      function,
      message: items
        .get("Information")
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Status={}", status.unwrap_or("<missing>"))),
    }),
  }
}

/// A short, single-line description of an error body for logs and
/// [`McwsError::Status`]: MC's `Information` item when the body is MCWS XML,
/// otherwise the first 200 characters.
pub(crate) fn error_detail(function: Function, body: &str) -> Option<String> {
  let trimmed = body.trim();
  if trimmed.is_empty() {
    return None;
  }
  match parse(function, trimmed) {
    Err(McwsError::Failure { message, .. }) => Some(message),
    _ => Some(
      trimmed
        .chars()
        .take(200)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect(),
    ),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // Captured from MC 36.0.51.
  const ALIVE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?>
<Response Status="OK">
<Item Name="RuntimeGUID">{F1255373-BCF8-4D88-A6DC-8D8B3C8248CA}</Item>
<Item Name="LibraryVersion">24</Item>
<Item Name="ProgramName">JRiver Media Center</Item>
<Item Name="ProgramVersion">36.0.51</Item>
<Item Name="FriendlyName">Desktop-9848rh4</Item>
<Item Name="ProductVersion">36 Windows</Item>
<Item Name="Platform">Windows</Item>
</Response>
"#;

  #[test]
  fn parses_ok_items_in_order() {
    let items = parse(Function::Alive, ALIVE).unwrap();
    assert_eq!(items.len(), 7);
    assert_eq!(items.get("ProgramVersion"), Some("36.0.51"));
    assert_eq!(items.get("Missing"), None);
  }

  #[test]
  fn decodes_xml_entities() {
    let body =
      r#"<Response Status="OK"><Item Name="Name">Tom &amp; Jerry &lt;3&gt;</Item></Response>"#;
    let items = parse(Function::PlaybackInfo, body).unwrap();
    assert_eq!(items.get("Name"), Some("Tom & Jerry <3>"));
  }

  #[test]
  fn empty_item_is_empty_string() {
    let body = r#"<Response Status="OK"><Item Name="Status"></Item><Item Name="X"/></Response>"#;
    let items = parse(Function::PlaybackInfo, body).unwrap();
    assert_eq!(items.get("Status"), Some(""));
    assert_eq!(items.get("X"), Some(""));
  }

  #[test]
  fn failure_status_carries_information() {
    let body =
      r#"<Response Status="Failure"><Item Name="Information">File not found</Item></Response>"#;
    let err = parse(Function::PlayByFilename, body).unwrap_err();
    assert!(
      matches!(&err, McwsError::Failure { function: Function::PlayByFilename, message } if message == "File not found"),
      "{err:?}"
    );
  }

  #[test]
  fn failure_without_information_names_the_status() {
    let body = r#"<Response Status="Failure"/>"#;
    let err = parse(Function::Stop, body).unwrap_err();
    assert!(err.to_string().contains("Status=Failure"), "{err}");
  }

  #[test]
  fn non_xml_and_wrong_root_are_parse_errors() {
    assert!(matches!(
      parse(Function::Alive, "<html>nope</html>"),
      Err(McwsError::Parse { .. })
    ));
    assert!(matches!(
      parse(Function::Alive, "not xml"),
      Err(McwsError::Parse { .. })
    ));
  }

  #[test]
  fn tolerates_byte_order_mark() {
    let body = "\u{feff}<Response Status=\"OK\"><Item Name=\"A\">1</Item></Response>";
    assert_eq!(parse(Function::Alive, body).unwrap().get("A"), Some("1"));
  }

  #[test]
  fn error_detail_prefers_mcws_message_and_truncates_other_bodies() {
    let xml = r#"<Response Status="Failure"><Item Name="Information">Bad path</Item></Response>"#;
    assert_eq!(
      error_detail(Function::PlayByFilename, xml).as_deref(),
      Some("Bad path")
    );
    assert_eq!(error_detail(Function::PlayByFilename, "   "), None);
    let long = "x".repeat(500);
    assert_eq!(
      error_detail(Function::PlayByFilename, &long).map(|s| s.len()),
      Some(200)
    );
  }
}
