fn normalize(s: &str) -> String {
    let lines: Vec<&str> = s.lines().map(|l| l.trim_end()).collect();
    let mut result: Vec<String> = Vec::new();
    let mut prev_blank = false;
    let mut import_block: Vec<&str> = Vec::new();

    for line in &lines {
        let is_import = is_import_line(line);

        if !is_import && !import_block.is_empty() {
            flush_imports(&mut import_block, &mut result);
        }

        if is_import {
            import_block.push(line);
            prev_blank = false;
            continue;
        }

        if line.is_empty() {
            if !prev_blank {
                result.push(String::new());
            }
            prev_blank = true;
        } else {
            result.push(line.to_string());
            prev_blank = false;
        }
    }

    if !import_block.is_empty() {
        flush_imports(&mut import_block, &mut result);
    }

    let out = result.join("\n");
    out.trim().to_string()
}

fn is_import_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("use ")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("from ")
        || trimmed.starts_with("#include")
        || trimmed.starts_with("require")
}

fn flush_imports(block: &mut Vec<&str>, out: &mut Vec<String>) {
    block.sort();
    out.extend(block.iter().map(|l| l.to_string()));
    block.clear();
}

fn word_diff(a: &str, b: &str) -> String {
    let words_a: Vec<&str> = a.split_whitespace().collect();
    let words_b: Vec<&str> = b.split_whitespace().collect();

    let n = words_a.len();
    let m = words_b.len();

    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if words_a[i] == words_b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && words_a[i] == words_b[j] {
            out.push_str(&format!("  {}\n", words_a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j >= m || dp[i + 1][j] >= dp[i][j + 1]) {
            out.push_str(&format!("- {}\n", words_a[i]));
            i += 1;
        } else {
            out.push_str(&format!("+ {}\n", words_b[j]));
            j += 1;
        }
    }
    out
}

pub fn assert_equivalent(native: &str, lua: &str) {
    let norm_native = normalize(native);
    let norm_lua = normalize(lua);

    if norm_native != norm_lua {
        let diff = word_diff(&norm_native, &norm_lua);
        panic!(
            "Normalized outputs differ.\n\
             \n--- native ---\n{norm_native}\n\
             \n--- lua ---\n{norm_lua}\n\
             \n--- word diff ---\n{diff}"
        );
    }
}
