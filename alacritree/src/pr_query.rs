//! Ask GitHub about many branches in one request.
//!
//! One `gh` process per worktree meant every worktree of a project asking the
//! same repository the same question, and all of them becoming due together
//! when the TTL expired.  Naming the exact head refs is what keeps the cost
//! proportional to branch count rather than to how many PRs the repository
//! has, which is what makes one request viable at all.

use std::collections::HashMap;

use crate::pr_status::PrInfo;

/// Aliases per request.  Measured: no error appeared at any size up to 398,
/// rate limit stays at one point through 100, and per-branch time flattens
/// around 50.  100 wins at the concurrency this pool actually leaves for
/// lookups, and is also under the ceiling a Windows command line would impose
/// if the query ever moved off stdin.
pub const CHUNK: usize = 100;

const FIELDS: &str = "number baseRefName url state isDraft headRepositoryOwner { login }";

/// `first: 5` because a head ref name matches across head repositories, so
/// several PRs can share one, and the owner tiebreak needs them all to choose
/// from.
///
/// `worktree::validate_branch_name` gates branches alacritree creates, but not
/// the ones a repository already had — those reach `build` unvalidated. One
/// query answers a whole project, so a single unescaped quote in any branch
/// name would break out of the string literal and take every branch's badge
/// down with it, not just its own. `graphql_string` is what closes that: a
/// JSON string literal is a valid GraphQL string literal, so encoding through
/// `serde_json` escapes exactly what GraphQL needs escaped.
pub fn build(owner: &str, name: &str, branches: &[String]) -> String {
    let mut q = format!(
        "query {{ repository(owner: {}, name: {}) {{",
        graphql_string(owner),
        graphql_string(name)
    );
    for (i, branch) in branches.iter().enumerate() {
        let branch = graphql_string(branch);
        q.push_str(&format!(
            " b{i}: pullRequests(headRefName: {branch}, states: [OPEN, MERGED, CLOSED], first: 5, \
             orderBy: {{field: CREATED_AT, direction: DESC}}) {{ nodes {{ {FIELDS} }} }}"
        ));
    }
    q.push_str(" } }");
    q
}

/// A GraphQL string literal for arbitrary UTF-8 input.  GraphQL's string
/// grammar is JSON's, so `serde_json`'s own quoting and escaping already does
/// the job.
fn graphql_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// `gh api graphql --input -` reads a JSON body, so the query is wrapped
/// rather than piped raw.
pub fn body(query: &str) -> String {
    serde_json::json!({ "query": query }).to_string()
}

/// Alias index back to branch, dropping any alias GitHub could not answer.
/// A partial response is normal here: one request covers a whole project, so
/// losing all of it because one branch failed would be worse than losing one.
///
/// `None` is the response that answered nothing — malformed, a null
/// `repository`, or every alias failing beside a top-level `errors`, all of
/// which GitHub can report under an HTTP 200 — and is what sends a group to
/// the per-branch path.  An empty map is the opposite: a well-formed answer
/// that no branch in this repository has a PR.  The two have to stay
/// distinguishable, or the common "nobody here has a PR" answer costs a
/// per-branch sweep that finds the same nothing.
pub fn parse(
    stdout: &[u8],
    branches: &[String],
    origin_owner: Option<&str>,
) -> Option<HashMap<String, PrInfo>> {
    let v: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let repo = v.pointer("/data/repository")?;
    if repo.is_null() {
        return None;
    }
    let mut found = HashMap::new();
    for (i, branch) in branches.iter().enumerate() {
        let Some(nodes) = repo.get(format!("b{i}")).and_then(|a| a.get("nodes")) else {
            continue;
        };
        let Some(list) = nodes.as_array() else { continue };
        if let Some(info) = crate::pr_status::select_and_build(list, origin_owner) {
            found.insert(branch.clone(), info);
        }
    }
    // Nothing came back and the response is carrying errors: a whole selection
    // failed, which a timeout on a full chunk produces.  A repository where no
    // branch has a PR answers the same shape without the `errors`, so reading
    // both as "no PR" would blank every badge in the project for a TTL.
    if found.is_empty() && has_errors(&v) {
        return None;
    }
    Some(found)
}

fn has_errors(response: &serde_json::Value) -> bool {
    response.get("errors").and_then(|e| e.as_array()).is_some_and(|list| !list.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_alias_per_branch() {
        let q = build("owner", "repo", &["main".into(), "topic".into()]);
        assert!(q.contains("b0: pullRequests(headRefName: \"main\""), "{q}");
        assert!(q.contains("b1: pullRequests(headRefName: \"topic\""), "{q}");
        assert!(q.contains("repository(owner: \"owner\", name: \"repo\")"), "{q}");
    }

    /// A quote in any interpolated name must stay inside its own literal
    /// rather than closing it early: one request answers a whole project, so a
    /// name that broke out would corrupt every branch's alias, not just its
    /// own.  The expected forms are spelled out rather than built with
    /// `graphql_string`, so a bug inside that function cannot satisfy both
    /// sides of the assertion.
    #[test]
    fn a_quote_in_any_name_stays_inside_its_own_literal() {
        let q = build("own\"er", "repo", &["topic\"evil".into(), "main".into()]);
        assert!(q.contains("repository(owner: \"own\\\"er\", name: \"repo\")"), "{q}");
        assert!(q.contains("b0: pullRequests(headRefName: \"topic\\\"evil\""), "{q}");
        assert!(q.contains("b1: pullRequests(headRefName: \"main\""), "{q}");
    }

    /// `gh api graphql --input -` reads a JSON body.  A bare query piped in comes
    /// back as HTTP 502, which reads like a transient GitHub failure and is not.
    #[test]
    fn the_body_is_json_wrapped() {
        let body = body("query { x }");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["query"], "query { x }");
    }

    /// One request answers for many branches, so an error on one alias must leave
    /// the rest usable rather than losing a whole project's badges.
    #[test]
    fn a_failed_alias_leaves_the_others() {
        let stdout = br#"{"data":{"repository":{
            "b0":{"nodes":[{"number":7,"baseRefName":"master","url":"u","state":"OPEN",
                            "isDraft":false,"headRepositoryOwner":{"login":"me"}}]},
            "b1":null}},
            "errors":[{"message":"something went wrong"}]}"#;
        let found = parse(stdout, &["main".into(), "topic".into()], Some("me"))
            .expect("a response carrying a repository is an answer");
        assert_eq!(found.get("main").map(|p| p.number), Some(7));
        assert!(!found.contains_key("topic"));
    }

    /// "No branch in this repository has a PR" is a real answer, and the
    /// caller has to be able to tell it from a request that failed: falling
    /// back on it would spend a `gh pr list` per branch finding the same
    /// nothing.
    #[test]
    fn a_response_with_no_prs_is_still_an_answer() {
        let stdout = br#"{"data":{"repository":{"b0":{"nodes":[]},"b1":{"nodes":[]}}}}"#;
        let found = parse(stdout, &["main".into(), "topic".into()], Some("me"));
        assert_eq!(found, Some(HashMap::new()));
    }

    /// GitHub answers a query it could not run with HTTP 200, a null
    /// `repository` and an `errors` list.  Read as "no PRs" that would blank
    /// every badge in the project until the TTL expired.
    #[test]
    fn a_null_repository_is_not_an_answer() {
        let stdout = br#"{"data":{"repository":null},
            "errors":[{"message":"Could not resolve to a Repository"}]}"#;
        assert!(parse(stdout, &["main".into()], Some("me")).is_none());
    }

    /// A whole-selection failure — a timeout on a full chunk, say — comes back
    /// as HTTP 200 with every alias null beside a top-level `errors`.  Read as
    /// "no branch here has a PR" it would blank every badge in the project
    /// until the TTL expired.
    #[test]
    fn a_response_whose_aliases_all_failed_is_not_an_answer() {
        let stdout = br#"{"data":{"repository":{"b0":null,"b1":null}},
            "errors":[{"message":"upstream timeout"}]}"#;
        assert!(parse(stdout, &["main".into(), "topic".into()], Some("me")).is_none());
    }

    #[test]
    fn malformed_output_is_not_an_answer() {
        assert!(parse(b"", &["main".into()], None).is_none());
        assert!(parse(b"not json", &["main".into()], None).is_none());
        assert!(parse(b"{}", &["main".into()], None).is_none());
    }

    /// A head ref name matches across head repositories, so several PRs can come
    /// back and the local origin's owner is what picks this checkout's own.
    #[test]
    fn the_origin_owner_breaks_a_tie() {
        let stdout = br#"{"data":{"repository":{"b0":{"nodes":[
            {"number":1,"baseRefName":"master","url":"a","state":"OPEN",
             "isDraft":false,"headRepositoryOwner":{"login":"someone-else"}},
            {"number":2,"baseRefName":"master","url":"b","state":"OPEN",
             "isDraft":false,"headRepositoryOwner":{"login":"me"}}]}}}}"#;
        let found = parse(stdout, &["main".into()], Some("me")).expect("an answer");
        assert_eq!(found.get("main").map(|p| p.number), Some(2));
    }
}
