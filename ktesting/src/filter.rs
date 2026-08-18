//! Glob-matching for test names: `*` matches any sequence including empty, `?`
//! exactly one byte. Byte-oriented — test names are ASCII module paths.

pub fn glob_match(pat: &[u8], name: &[u8]) -> bool {
    let mut pi = 0usize;
    let mut ni = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_ni = 0usize;

    while ni < name.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == name[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            star_ni = ni;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ni += 1;
            ni = star_ni;
        } else {
            return false;
        }
    }

    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }

    pi == pat.len()
}

pub fn matches_any(pats: &[&[u8]], name: &[u8]) -> bool {
    pats.iter().any(|p| glob_match(p, name))
}
