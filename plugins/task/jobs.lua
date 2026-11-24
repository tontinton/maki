-- Session job counts for the status bar. The count is taken from joblist on
-- every event, so a reload starts with the right number and no local table can
-- go stale. The JobStart/JobExit events only say when to recount.

local SEGMENT_ID = "jobs"

local M = {}

function M.publish(session)
  if not session then
    return
  end
  local n = 0
  for _, job in ipairs(maki.fn.joblist(session) or {}) do
    if job.status == "running" then
      n = n + 1
    end
  end
  if n == 0 then
    maki.ui.status_segment({ id = SEGMENT_ID, session = session, remove = true })
    return
  end
  maki.ui.status_segment({
    id = SEGMENT_ID,
    session = session,
    text = string.format("%d %s running", n, n == 1 and "job" or "jobs"),
  })
end

maki.api.create_autocmd("JobStart", {
  callback = function(ev)
    M.publish(ev.data.session_id)
  end,
})

maki.api.create_autocmd("JobExit", {
  callback = function(ev)
    M.publish(ev.data.session_id)
  end,
})

maki.api.create_autocmd("SessionEnd", {
  callback = function(ev)
    if ev.data.session_id then
      maki.ui.status_segment({ id = SEGMENT_ID, session = ev.data.session_id, remove = true })
    end
  end,
})

return M
