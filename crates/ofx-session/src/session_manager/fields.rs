use std::borrow::Cow;

use super::json::{self, Value};

const MAX_SPLIT_FIELDS: usize = 24;

pub(crate) trait Tag: Copy + 'static {
    const ALL: &'static [Self];

    fn name(self) -> &'static str;

    fn from_text(text: &str) -> Option<Self> {
        if let Some(tag) = Self::ALL.iter().copied().find(|tag| tag.name() == text) {
            return Some(tag);
        }
        if text == "-0" || text.contains(['.', 'e', 'E']) {
            return None;
        }
        let index = json::slice_to_u64(text)?;
        Self::ALL.get(usize::try_from(index).ok()?).copied()
    }
}

pub(crate) struct Fields<'a> {
    pairs: Vec<(Cow<'a, str>, &'a [u8])>,
}

impl<'a> Fields<'a> {
    pub(crate) fn parse_object(object: &'a [u8]) -> Option<Self> {
        split_object(object).or_else(|| parse_object_strict(object))
    }

    pub(crate) fn raw(&self, name: &str) -> Option<&'a [u8]> {
        self.pairs
            .iter()
            .find(|(field, _)| field == name)
            .map(|&(_, value)| value)
    }

    fn present(&self, name: &str) -> Option<&'a [u8]> {
        self.raw(name).filter(|value| *value != b"null")
    }

    pub(crate) fn opt_u64(&self, name: &str) -> Result<Option<u64>, BadBody> {
        let Some(value) = self.present(name) else {
            return Ok(None);
        };
        fast_u64(value)
            .or_else(|| full_u64(value))
            .map(Some)
            .ok_or(BadBody)
    }

    pub(crate) fn req_u64(&self, name: &str) -> Result<u64, BadBody> {
        self.opt_u64(name)?.ok_or(BadBody)
    }

    pub(crate) fn opt_str(&self, name: &str) -> Result<Option<Cow<'a, str>>, BadBody> {
        let Some(value) = self.present(name) else {
            return Ok(None);
        };
        plain_string(value)
            .map(Cow::Borrowed)
            .or_else(|| full_string(value).map(Cow::Owned))
            .map(Some)
            .ok_or(BadBody)
    }

    pub(crate) fn req_str(&self, name: &str) -> Result<Cow<'a, str>, BadBody> {
        self.opt_str(name)?.ok_or(BadBody)
    }

    pub(crate) fn req_tag<T: Tag>(&self, name: &str) -> Result<T, BadBody> {
        let value = self.present(name).ok_or(BadBody)?;
        if let Some(tag) = plain_string(value)
            .and_then(|text| T::ALL.iter().copied().find(|tag| tag.name() == text))
        {
            return Ok(tag);
        }
        Self::tag_value(value)
    }

    pub(crate) fn opt_tag<T: Tag>(&self, name: &str) -> Result<Option<T>, BadBody> {
        if self.present(name).is_none() {
            return Ok(None);
        }
        self.req_tag(name).map(Some)
    }

    pub(crate) fn req_bool(&self, name: &str) -> Result<bool, BadBody> {
        match json::parse(self.present(name).ok_or(BadBody)?) {
            Some(Value::Bool(value)) => Ok(value),
            _ => Err(BadBody),
        }
    }

    pub(crate) fn opt_str_list(&self, name: &str) -> Result<Option<Vec<String>>, BadBody> {
        let Some(value) = self.present(name) else {
            return Ok(None);
        };
        let Some(Value::Array(elements)) = json::parse(value) else {
            return Err(BadBody);
        };
        elements
            .into_iter()
            .map(|element| full_string(element).ok_or(BadBody))
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    pub(crate) fn opt_struct(
        &self,
        name: &str,
        members: &[&str],
    ) -> Result<Option<Vec<&'a [u8]>>, BadBody> {
        let Some(value) = self.present(name) else {
            return Ok(None);
        };
        let Some(Value::Object) = json::parse(value) else {
            return Err(BadBody);
        };
        let start = json::skip_whitespace(value, 0);
        let object = Self::parse_object(&value[start..]).ok_or(BadBody)?;
        let mut found: Vec<Option<&'a [u8]>> = vec![None; members.len()];
        for (field, raw) in object.pairs {
            let slot = members
                .iter()
                .position(|member| *member == field)
                .ok_or(BadBody)?;
            if found[slot].replace(raw).is_some() {
                return Err(BadBody);
            }
        }
        found
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or(BadBody)
    }

    pub(crate) fn string_value(raw: &[u8]) -> Result<String, BadBody> {
        full_string(raw).ok_or(BadBody)
    }

    pub(crate) fn u64_value(raw: &[u8]) -> Result<u64, BadBody> {
        full_u64(raw).ok_or(BadBody)
    }

    pub(crate) fn tag_value<T: Tag>(raw: &[u8]) -> Result<T, BadBody> {
        match json::parse(raw).ok_or(BadBody)? {
            Value::String(text) => T::from_text(&text),
            Value::Number(text) => T::from_text(text),
            _ => None,
        }
        .ok_or(BadBody)
    }

    pub(crate) fn req_u64_list(&self, name: &str) -> Result<Vec<u64>, BadBody> {
        let value = self.present(name).ok_or(BadBody)?;
        let Value::Array(elements) = json::parse(value).ok_or(BadBody)? else {
            return Err(BadBody);
        };
        elements
            .into_iter()
            .map(|element| full_u64(element).ok_or(BadBody))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BadBody;

pub(crate) fn full_string(value: &[u8]) -> Option<String> {
    match json::parse(value)? {
        Value::String(text) => Some(text),
        Value::Array(elements) => {
            let bytes = elements
                .into_iter()
                .map(|element| u8::try_from(full_u64(element)?).ok())
                .collect::<Option<Vec<u8>>>()?;
            String::from_utf8(bytes).ok()
        }
        _ => None,
    }
}

fn full_u64(value: &[u8]) -> Option<u64> {
    match json::parse(value)? {
        Value::Number(text) => json::slice_to_u64(text),
        Value::String(text) => json::slice_to_u64(&text),
        _ => None,
    }
}

fn fast_u64(value: &[u8]) -> Option<u64> {
    if value.is_empty()
        || value.len() > 19
        || !value.iter().all(u8::is_ascii_digit)
        || (value.len() > 1 && value[0] == b'0')
    {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn plain_string(value: &[u8]) -> Option<&str> {
    let inner = value.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    if inner.contains(&b'\\') {
        return None;
    }
    std::str::from_utf8(inner).ok()
}

fn split_object(object: &[u8]) -> Option<Fields<'_>> {
    if object.len() < 2 || object[0] != b'{' || object[object.len() - 1] != b'}' {
        return None;
    }
    let mut pairs = Vec::new();
    if object.len() == 2 {
        return Some(Fields { pairs });
    }
    let mut at = 1;
    loop {
        if pairs.len() == MAX_SPLIT_FIELDS || object[at] != b'"' {
            return None;
        }
        let name_end = at + 1 + object[at + 1..].iter().position(|&byte| byte == b'"')?;
        let name = &object[at + 1..name_end];
        if name.contains(&b'\\') {
            return None;
        }
        at = name_end + 1;
        if object.get(at) != Some(&b':') {
            return None;
        }
        at += 1;
        let value_end = split_value_end(object, at)?;
        if value_end == at {
            return None;
        }
        pairs.push((
            Cow::Borrowed(std::str::from_utf8(name).ok()?),
            &object[at..value_end],
        ));
        at = value_end;
        if at == object.len() - 1 {
            return Some(Fields { pairs });
        }
        if object[at] != b',' {
            return None;
        }
        at += 1;
    }
}

fn split_value_end(bytes: &[u8], at: usize) -> Option<usize> {
    match *bytes.get(at)? {
        b'"' => split_string_end(bytes, at),
        b'{' | b'[' => {
            let mut depth = 0_usize;
            let mut index = at;
            while index < bytes.len() {
                match bytes[index] {
                    b'"' => {
                        index = split_string_end(bytes, index)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(index + 1);
                        }
                    }
                    _ => {}
                }
                index += 1;
            }
            None
        }
        _ => {
            let end = at
                + bytes[at..]
                    .iter()
                    .position(|&byte| byte == b',' || byte == b'}')?;
            Some(end)
        }
    }
}

fn split_string_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut from = open + 1;
    loop {
        let quote = from + bytes[from..].iter().position(|&byte| byte == b'"')?;
        let run = bytes[open + 1..quote]
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\\')
            .count();
        if run % 2 == 0 {
            return Some(quote + 1);
        }
        from = quote + 1;
    }
}

fn parse_object_strict(object: &[u8]) -> Option<Fields<'_>> {
    let start = json::skip_whitespace(object, 0);
    if object.get(start) != Some(&b'{') {
        return None;
    }
    json::value_end(object, start)?;
    let mut pairs = Vec::new();
    let mut at = json::skip_whitespace(object, start + 1);
    if object[at] == b'}' {
        return Some(Fields { pairs });
    }
    loop {
        let name_end = json::string_end(object, at)?;
        let name = json::decode_string(&object[at..name_end])?;
        let value_start =
            json::skip_whitespace(object, json::skip_whitespace(object, name_end) + 1);
        let value_end = json::value_end(object, value_start)?;
        pairs.push((Cow::Owned(name), &object[value_start..value_end]));
        at = json::skip_whitespace(object, value_end);
        match object[at] {
            b',' => at = json::skip_whitespace(object, at + 1),
            _ => return Some(Fields { pairs }),
        }
    }
}

#[cfg(test)]
mod tests;
