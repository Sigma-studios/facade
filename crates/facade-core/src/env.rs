//! References between apps' variables: `${{db.DATABASE_URL}}` in one app's value is
//! replaced by the `DATABASE_URL` of app `db` when the app is started, so a
//! password lives in one place.

/// One `${{app.KEY}}` found in a value: where it is and what it names.
struct Reference<'a> {
    start: usize,
    end: usize,
    app: &'a str,
    key: &'a str,
}

fn scan(value: &str) -> Vec<Reference<'_>> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(i) = value[from..].find("${{") {
        let start = from + i;
        let Some(len) = value[start + 3..].find("}}") else {
            break;
        };
        let end = start + 3 + len + 2;
        let inner = value[start + 3..start + 3 + len].trim();
        // `app.KEY`, or `stack.service.KEY` for a stack's service.
        if let Some((app, key)) = inner.rsplit_once('.')
            && crate::validate_app_name(app).is_ok()
            && is_key(key)
        {
            found.push(Reference {
                start,
                end,
                app,
                key,
            });
        }
        from = end;
    }
    found
}

fn is_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `(app, KEY)` pairs referenced by `value`.
pub fn references(value: &str) -> Vec<(String, String)> {
    scan(value)
        .into_iter()
        .map(|r| (r.app.to_string(), r.key.to_string()))
        .collect()
}

/// `value` with every reference replaced by what `lookup(app, KEY)` returns.
pub fn substitute(
    value: &str,
    mut lookup: impl FnMut(&str, &str) -> Result<String, String>,
) -> Result<String, String> {
    let refs = scan(value);
    if refs.is_empty() {
        return Ok(value.to_string());
    }
    let mut out = String::with_capacity(value.len());
    let mut last = 0;
    for r in refs {
        out.push_str(&value[last..r.start]);
        out.push_str(&lookup(r.app, r.key)?);
        last = r.end;
    }
    out.push_str(&value[last..]);
    Ok(out)
}

/// `${{app.KEY}}`.
pub fn reference(app: &str, key: &str) -> String {
    format!("${{{{{app}.{key}}}}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes() {
        let v = "postgres://postgres:${{db.POSTGRES_PASSWORD}}@db:5432/${{ db.NAME }}";
        assert_eq!(
            references(v),
            [
                ("db".to_string(), "POSTGRES_PASSWORD".to_string()),
                ("db".to_string(), "NAME".to_string())
            ]
        );
        let out = substitute(v, |app, key| Ok(format!("<{app}:{key}>"))).unwrap();
        assert_eq!(
            out,
            "postgres://postgres:<db:POSTGRES_PASSWORD>@db:5432/<db:NAME>"
        );
        // Not references: left alone.
        assert_eq!(
            substitute("${{nope}} ${HOME}", |_, _| Err("x".into())).unwrap(),
            "${{nope}} ${HOME}"
        );
        assert_eq!(reference("db", "URL"), "${{db.URL}}");
        assert_eq!(
            references("${{shop.db.URL}}"),
            [("shop.db".to_string(), "URL".to_string())]
        );
    }
}
