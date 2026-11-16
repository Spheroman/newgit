//! `GET /v1/serve/status`: what an operator of this server looks at when
//! something runs in the background and fails there, not in a request: the
//! landing log, stored rooted histories, and garbage collection.
//! Admins only (`--admin`): it names every project.

use serde_json::{Value, json};

use crate::App;
use crate::api::{ApiResponse, Caller};
use crate::db::DbGuard;
use crate::error::{ApiResult, fail};
use crate::tokens;

pub fn status(app: &App, db: DbGuard<'_>, caller: &Caller) -> ApiResult<ApiResponse> {
    // Nothing here is read from SQLite, and the landing queue is never
    // waited on with it held.
    drop(db);
    if caller.kind == "agent" || !tokens::listed(&app.config.admin_logins, &caller.login) {
        return fail(403, "only this server's admins (`--admin`) read its status");
    }
    let stuck: Vec<Value> = app
        .log
        .stuck()
        .into_iter()
        .map(|stuck| {
            json!({
                "stamp": stuck.stamp,
                "by": stuck.entry.by,
                "homes": stuck.entry.homes.keys().collect::<Vec<_>>(),
                "cause": stuck.cause,
                "placed": stuck.placed,
            })
        })
        .collect();
    let failing: Vec<Value> = app
        .rooted
        .failing()
        .into_iter()
        .map(|failing| {
            json!({
                "home": failing.home,
                "error": failing.error,
                "attempts": failing.attempts,
            })
        })
        .collect();
    let backfilled = app
        .rooted
        .backfilled
        .load(std::sync::atomic::Ordering::SeqCst);
    let maintenance = app.maintenance.last().map(|pass| {
        json!({
            "started": pass.started,
            "finished": pass.finished,
            "stores": pass.stores,
            "landings_removed": pass.landings_removed,
            "failed": pass.failed,
        })
    });
    Ok(ApiResponse {
        status: 200,
        body: json!({
            "landing": {
                "tip": app.log.tip(),
                "applied": app.log.applied(),
                "stuck": stuck,
            },
            "rooted": { "backfilled": backfilled, "failing": failing },
            "gc": { "last": maintenance },
        }),
    })
}
