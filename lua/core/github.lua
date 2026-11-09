-- GitHub: pull requests, reviews, checks and issues, through the gh CLI
-- (https://cli.github.com; `gh auth login` once). Everything runs in the
-- background; the buffers redraw when gh answers.
--
-- SPC g p   pull requests (*github-prs*)    SPC g i   issues (*github-issues*)
-- SPC g n   a pull request for this branch  SPC g I   a new issue
-- In *git-status*: @ p, @ i, @ n do the same, and the branch's pull
-- request shows under the head, with its checks and review.
--
-- In a list: RET opens one, b opens it in the browser, a shows all (open,
-- closed, merged) or open ones again, g r refreshes, q goes back; in the
-- pull requests also C checks one out and d reviews its changes.
--
-- A pull request (*github-pr*): its description, checks, reviews and
-- conversation.
--   d  review its changes       c  comment         a  approve
--   r  request changes          m  merge (asks how) C  check it out
--   b  in the browser           g r refresh        q  back
--
-- Reviewing (*github-pr-diff*, drawn with gpui-kit's Diff, its comments
-- beside their lines): c (or the + beside a line) writes a comment on the
-- line under the cursor, kept until you submit; C deletes yours there;
-- S submits the review: a comment, an approval or a request for changes,
-- with your line comments. RET visits the line.
--
-- An issue (*github-issue*): c comment, x close or reopen, b browser.
--
-- New pull requests and issues are written in a buffer: the first line is
-- the title, the rest the description; C-c C-c creates it (C-c C-d: a
-- draft pull request), C-c C-k cancels. The branch is pushed first if it
-- has no upstream yet.

local V = vmac
local G = core.git
local H = {}
core.github = H

-- The program: gh. (Tests put a stand-in here.)
H.program = "gh"

H.prs_buffer = "*github-prs*"
H.pr_buffer = "*github-pr*"
H.diff_buffer = "*github-pr-diff*"
H.issues_buffer = "*github-issues*"
H.issue_buffer = "*github-issue*"
H.new_buffer = "*github-new*"

local buffers = {}
for _, name in ipairs({ H.prs_buffer, H.pr_buffer, H.diff_buffer, H.issues_buffer, H.issue_buffer, H.new_buffer }) do
  buffers[name] = true
end

local pieces, line, lines_of = G.pieces, G.line, G.lines_of

function H.in_buffer() return buffers[V.buffer_name()] == true end

-- The repository: the one a GitHub buffer shows, else the current file's.
function H.root()
  if buffers[V.buffer_name()] and H.repo_root then return H.repo_root end
  local root = G.root()
  if root then H.repo_root = root end
  return root
end

---- Running gh

local jobs, next_job = {}, 0

-- Runs gh in the background; done(ok, output, errors).
function H.gh(args, done, stdin, busy)
  local root = H.repo_root or H.root()
  if not root then return end
  next_job = next_job + 1
  local tag = tostring(next_job)
  jobs[tag] = done
  if busy then V.message(busy) end
  V.spawn(root, H.program, args, stdin, tag)
end

core.events["spawn-done"] = function(args)
  local done = jobs[args[1]]
  jobs[args[1]] = nil
  if done then done(args[2] == "ok", args[3], args[4]) end
end

-- gh with --json: done(value), or a message saying what went wrong.
function H.gh_json(args, done, busy)
  H.gh(args, function(ok, out, err)
    if not ok then return V.message("gh: " .. G.last_line(err ~= "" and err or out)) end
    local value, why = V.json_decode(out)
    if value == nil and why then return V.message("gh: " .. why) end
    done(value or {})
  end, nil, busy)
end

---- Small things

-- "2026-11-08T12:00:00Z" -> "3 days ago".
function H.ago(iso)
  local y, mo, d, h, mi, s = (iso or ""):match("(%d+)-(%d+)-(%d+)T(%d+):(%d+):(%d+)")
  if not y then return "" end
  local utc = os.time({ year = tonumber(y), month = tonumber(mo), day = tonumber(d), hour = tonumber(h), min = tonumber(mi), sec = tonumber(s) })
  local offset = os.time() - os.time(os.date("!*t"))
  local secs = math.max(0, os.time() - (utc + offset))
  local units = { { 31536000, "year" }, { 2592000, "month" }, { 86400, "day" }, { 3600, "hour" }, { 60, "minute" } }
  for _, u in ipairs(units) do
    if secs >= u[1] then
      local n = secs // u[1]
      return n .. " " .. u[2] .. (n == 1 and "" or "s") .. " ago"
    end
  end
  return "just now"
end

local function login(user) return user and user.login and ("@" .. user.login) or "@ghost" end

-- The checks' state, from statusCheckRollup: mark, face, words.
function H.checks(rollup)
  if type(rollup) ~= "table" or #rollup == 0 then return " ", nil, "no checks" end
  local failed, pending, passed = 0, 0, 0
  for _, c in ipairs(rollup) do
    local result = c.conclusion or c.state or ""
    local status = c.status or ""
    if result == "FAILURE" or result == "ERROR" or result == "TIMED_OUT" or result == "CANCELLED" or result == "ACTION_REQUIRED" then
      failed = failed + 1
    elseif status ~= "" and status ~= "COMPLETED" or result == "PENDING" or result == "EXPECTED" or (result == "" and status == "") then
      pending = pending + 1
    else
      passed = passed + 1
    end
  end
  if failed > 0 then return "✗", "red", failed .. " failing" end
  if pending > 0 then return "•", "yellow", pending .. " running" end
  return "✓", "green", "checks pass"
end

local function check_name(c)
  return c.name or c.context or "check"
end

local function check_mark(c)
  local mark, face = H.checks({ c })
  return mark, face
end

local review_words = {
  APPROVED = { "approved", "green" },
  CHANGES_REQUESTED = { "changes requested", "red" },
  REVIEW_REQUIRED = { "review required", "yellow" },
}

local function body_lines(out, text, indent)
  indent = indent or ""
  local body = (text or ""):gsub("\r", "")
  if body:match("^%s*$") then
    out[#out + 1] = line(indent .. "(no description)", "dim")
    return
  end
  for _, l in ipairs(lines_of(body)) do out[#out + 1] = line(indent .. l) end
end

-- Remembers which buffer to go back to when a GitHub buffer opens.
local function open_buffer(name)
  if not buffers[V.buffer_name()] then H.return_to = V.buffer_name() end
  V.ensure_buffer(name)
  V.switch_to_buffer(name)
end

function H.close()
  local name = V.buffer_name()
  -- A pull request or issue goes back to its list; a list to where it came from.
  if name == H.diff_buffer and V.switch_to_buffer(H.pr_buffer) then return end
  if name == H.pr_buffer and H.list_kind == "pr" and V.switch_to_buffer(H.prs_buffer) then return end
  if name == H.issue_buffer and H.list_kind == "issue" and V.switch_to_buffer(H.issues_buffer) then return end
  if H.return_to and V.switch_to_buffer(H.return_to) then return end
  V.next_buffer(1)
end

---- Lists: pull requests and issues

H.show_all = { pr = false, issue = false }
H.list_items = {}

local pr_fields = "number,title,author,headRefName,isDraft,reviewDecision,statusCheckRollup,updatedAt,state"
local issue_fields = "number,title,author,labels,updatedAt,state,comments"

function H.list(kind, keep)
  local root = H.root()
  if not root then return end
  H.repo_root = root
  local name = kind == "pr" and H.prs_buffer or H.issues_buffer
  local state = H.show_all[kind] and "all" or "open"
  local args = { kind, "list", "--state", state, "--limit", "100", "--json", kind == "pr" and pr_fields or issue_fields }
  if not keep then
    G.draw(name, { line((kind == "pr" and "Pull requests" or "Issues") .. " · loading…", "git-section") })
    open_buffer(name)
  end
  H.gh_json(args, function(list)
    local out, items = {}, {}
    local title = (kind == "pr" and "Pull requests" or "Issues") .. " (" .. #list .. (state == "open" and " open" or "") .. ")"
    out[1] = line(title, "git-section")
    out[2] = line("")
    for _, it in ipairs(list) do
      local parts = { { string.format("#%-5d", it.number), "git-hash" } }
      if kind == "pr" then
        local mark, face = H.checks(it.statusCheckRollup)
        parts[#parts + 1] = { mark .. " ", face }
      end
      if it.state and it.state ~= "OPEN" then parts[#parts + 1] = { it.state:lower() .. " ", "dim" } end
      if it.isDraft then parts[#parts + 1] = { "draft ", "dim" } end
      parts[#parts + 1] = { it.title }
      parts[#parts + 1] = { "  " .. login(it.author), "git-ref" }
      if kind == "pr" then
        parts[#parts + 1] = { "  " .. (it.headRefName or ""), "git-branch" }
        local words = review_words[it.reviewDecision or ""]
        if words then parts[#parts + 1] = { "  " .. words[1], words[2] } end
      else
        for _, l in ipairs(it.labels or {}) do parts[#parts + 1] = { "  [" .. l.name .. "]", "magenta" } end
        local n = type(it.comments) == "table" and #it.comments or 0
        if n > 0 then parts[#parts + 1] = { "  💬 " .. n, "dim" } end
      end
      parts[#parts + 1] = { "  " .. H.ago(it.updatedAt), "dim" }
      out[#out + 1] = pieces(parts)
      items[#out] = it.number
    end
    if #list == 0 then out[#out + 1] = line(state == "open" and "None open (a shows all)" or "None", "dim") end
    out[#out + 1] = line("")
    local keys = kind == "pr" and "RET open · d review · C check out · b browser · n new · a all/open · g r refresh · q back"
      or "RET open · b browser · n new · a all/open · g r refresh · q back"
    out[#out + 1] = line(keys, "dim")
    H.list_items[kind] = items
    G.draw(name, out, V.buffer_name() == name and V.line_number() or nil)
    if V.buffer_name() == name and not keep then V.goto_line_column(3, 1) end
    V.message("")
  end, "Asking GitHub…")
end

local function number_here()
  local name = V.buffer_name()
  if name == H.prs_buffer then return H.list_items.pr and H.list_items.pr[V.line_number()], "pr" end
  if name == H.issues_buffer then return H.list_items.issue and H.list_items.issue[V.line_number()], "issue" end
  if name == H.pr_buffer or name == H.diff_buffer then return H.pr and H.pr.number, "pr" end
  if name == H.issue_buffer then return H.issue and H.issue.number, "issue" end
  -- In the status: the branch's pull request.
  local cached = H.branch_pr_cached()
  return cached and cached.number, "pr"
end

function H.toggle_all()
  local kind = V.buffer_name() == H.prs_buffer and "pr" or "issue"
  H.show_all[kind] = not H.show_all[kind]
  H.list(kind, true)
end

function H.list_refresh()
  local name = V.buffer_name()
  if name == H.prs_buffer then return H.list("pr", true) end
  if name == H.issues_buffer then return H.list("issue", true) end
  if name == H.pr_buffer then return H.view_pr(H.pr.number, true) end
  if name == H.issue_buffer then return H.view_issue(H.issue.number, true) end
  if name == H.diff_buffer then return H.review(H.pr.number) end
end

function H.open_here()
  local n, kind = number_here()
  if not n then return V.message("Not on a " .. (kind == "pr" and "pull request" or "issue")) end
  if kind == "pr" then H.view_pr(n) else H.view_issue(n) end
end

function H.browse()
  local n, kind = number_here()
  if not n then
    return H.gh({ "browse" }, function() end)
  end
  H.gh({ kind, "view", tostring(n), "--web" }, function(ok, _, err)
    if not ok then V.message("gh: " .. G.last_line(err)) end
  end)
end

---- A pull request

local view_fields = "number,title,body,author,state,isDraft,baseRefName,headRefName,url,additions,deletions,"
  .. "changedFiles,reviewDecision,statusCheckRollup,comments,reviews,mergeable,headRefOid,createdAt"

-- The conversation: comments and reviews, oldest first.
local function conversation(out, comments, reviews)
  local all = {}
  for _, c in ipairs(comments or {}) do all[#all + 1] = { at = c.createdAt or "", who = c.author, body = c.body } end
  for _, r in ipairs(reviews or {}) do
    local state = ({ APPROVED = "approved", CHANGES_REQUESTED = "requested changes", COMMENTED = "reviewed", DISMISSED = "review dismissed" })[r.state] or (r.state or ""):lower()
    if state ~= "" or (r.body or "") ~= "" then
      all[#all + 1] = { at = r.submittedAt or "", who = r.author, body = r.body, state = state, raw = r.state }
    end
  end
  table.sort(all, function(a, b) return a.at < b.at end)
  out[#out + 1] = line("Conversation (" .. #all .. ")", "git-section")
  for _, c in ipairs(all) do
    local parts = { { "  " .. login(c.who), "git-ref" } }
    if c.state then
      local face = c.raw == "APPROVED" and "green" or c.raw == "CHANGES_REQUESTED" and "red" or "dim"
      parts[#parts + 1] = { " " .. c.state, face }
    end
    parts[#parts + 1] = { " · " .. H.ago(c.at), "dim" }
    out[#out + 1] = pieces(parts)
    if (c.body or ""):match("%S") then body_lines(out, c.body, "    ") end
    out[#out + 1] = line("")
  end
end

function H.view_pr(n, keep)
  if not keep then
    G.draw(H.pr_buffer, { line("#" .. n .. " · loading…", "git-section") })
    open_buffer(H.pr_buffer)
  end
  H.gh_json({ "pr", "view", tostring(n), "--json", view_fields }, function(pr)
    H.pr = pr
    local out = {}
    out[#out + 1] = pieces({ { "#" .. pr.number .. " ", "git-hash" }, { pr.title or "", "git-section" } })
    local state = pr.isDraft and "Draft" or (pr.state or ""):sub(1, 1) .. (pr.state or ""):sub(2):lower()
    out[#out + 1] = pieces({
      { state, pr.state == "MERGED" and "magenta" or pr.state == "CLOSED" and "red" or "green" },
      { " · " .. login(pr.author) .. " wants to merge " },
      { pr.headRefName or "", "git-branch" }, { " into " }, { pr.baseRefName or "", "git-branch" },
      { string.format("  +%d −%d in %d files", pr.additions or 0, pr.deletions or 0, pr.changedFiles or 0), "dim" },
    })
    local words = review_words[pr.reviewDecision or ""]
    local mergeable = ({ MERGEABLE = "can merge", CONFLICTING = "has conflicts", UNKNOWN = "" })[pr.mergeable or ""] or ""
    out[#out + 1] = pieces({ { "Review: ", "git-label" }, { words and words[1] or "none yet", words and words[2] or "dim" },
      { mergeable ~= "" and ("  ·  " .. mergeable) or "", pr.mergeable == "CONFLICTING" and "red" or "dim" } })
    out[#out + 1] = line(pr.url or "", "dim")
    out[#out + 1] = line("")
    local mark, face, summary = H.checks(pr.statusCheckRollup)
    out[#out + 1] = pieces({ { "Checks ", "git-section" }, { mark .. " " .. summary, face } })
    for _, c in ipairs(pr.statusCheckRollup or {}) do
      local m, f = check_mark(c)
      out[#out + 1] = pieces({ { "  " .. m .. " ", f }, { check_name(c) }, { c.workflowName and ("  " .. c.workflowName) or "", "dim" } })
    end
    out[#out + 1] = line("")
    out[#out + 1] = line("Description", "git-section")
    body_lines(out, pr.body, "  ")
    out[#out + 1] = line("")
    conversation(out, pr.comments, pr.reviews)
    out[#out + 1] = line("d review · c comment · a approve · r request changes · m merge · C check out · b browser · g r refresh · q back", "dim")
    G.draw(H.pr_buffer, out, V.buffer_name() == H.pr_buffer and keep and V.line_number() or nil)
    if V.buffer_name() == H.pr_buffer and not keep then V.goto_line_column(1, 1) end
    V.message("")
  end, "Asking GitHub…")
end

-- Writes a comment, then runs submit(text) when it's not empty.
local function ask_text(prompt, submit, allow_empty)
  V.prompt(prompt, function(text)
    if text == "" and not allow_empty then return V.message("Nothing sent") end
    submit(text)
  end)
end

function H.comment()
  local n, kind = number_here()
  if not n then return V.message("Open a pull request or issue first") end
  ask_text("Comment on #" .. n .. ": ", function(text)
    H.gh({ kind, "comment", tostring(n), "--body-file", "-" }, function(ok, _, err)
      if not ok then return V.message("gh: " .. G.last_line(err)) end
      V.message("Commented on #" .. n)
      H.list_refresh()
    end, text, "Sending…")
  end)
end

local function pr_review(event, word)
  return function()
    local n = number_here()
    if not n then return V.message("Open a pull request first") end
    ask_text((event == "approve" and "Approve" or "Request changes on") .. " #" .. n .. (event == "approve" and " (a note, or RET): " or ": "), function(text)
      local args = { "pr", "review", tostring(n), "--" .. event }
      if text ~= "" then
        args[#args + 1] = "--body"
        args[#args + 1] = text
      end
      H.gh(args, function(ok, _, err)
        if not ok then return V.message("gh: " .. G.last_line(err)) end
        V.message(word .. " #" .. n)
        H.list_refresh()
      end, nil, "Sending…")
    end, event == "approve")
  end
end
H.approve = pr_review("approve", "Approved")
H.request_changes = pr_review("request-changes", "Requested changes on")

function H.merge()
  local n = number_here()
  if not n then return V.message("Open a pull request first") end
  local ways = { "1. Squash and merge", "2. Create a merge commit", "3. Rebase and merge" }
  core.pick("Merge #" .. n, ways, "", function(choice)
    local flag = ({ "--squash", "--merge", "--rebase" })[core.leading_number(choice) or 1]
    core.confirm("Merge #" .. n .. " (" .. choice:sub(4):lower() .. ") and delete its branch?", function()
      H.gh({ "pr", "merge", tostring(n), flag, "--delete-branch" }, function(ok, out, err)
        if not ok then return V.message("gh: " .. G.last_line(err ~= "" and err or out)) end
        V.git_forget()
        V.message("Merged #" .. n)
        H.list_refresh()
      end, nil, "Merging…")
    end)
  end)
end

function H.checkout()
  local n = number_here()
  if not n then return V.message("Not on a pull request") end
  H.gh({ "pr", "checkout", tostring(n) }, function(ok, out, err)
    if not ok then return V.message("gh: " .. G.last_line(err ~= "" and err or out)) end
    V.git_forget()
    H.branch_prs = {}
    V.message("Checked out #" .. n .. " (" .. V.git_branch(H.repo_root) .. ")")
  end, nil, "Checking out #" .. n .. "…")
end

---- Reviewing a pull request's changes

-- Comments of yours not sent yet, by pull request: { path, side, line, body }.
H.pending = {}
H.thread = {}

local function show_comments()
  local list = {}
  for _, c in ipairs(H.thread) do list[#list + 1] = { c.path, c.side, tostring(c.line), c.who, c.body } end
  for _, c in ipairs(H.pending[H.pr.number] or {}) do
    list[#list + 1] = { c.path, c.side, tostring(c.line), "pending", c.body }
  end
  V.set_buffer_diff_comments(H.diff_buffer, list)
end

-- d: the pull request's changes in a Diff, with the comments on them.
function H.review(n)
  n = n or number_here()
  if not n then return V.message("Not on a pull request") end
  local function with_pr(pr)
    H.pr = pr
    H.gh({ "pr", "diff", tostring(n), "--color", "never" }, function(ok, out, err)
      if not ok then return V.message("gh: " .. G.last_line(err)) end
      local header = "Reviewing #" .. n .. " " .. (pr.title or "") .. "\n"
        .. "c comments on a line · C deletes yours · S submits the review · RET visits · q back\n"
      G.draw(H.diff_buffer, { line("") })
      V.set_buffer_read_only(H.diff_buffer, false)
      V.set_buffer_text(H.diff_buffer, header .. "\n" .. out)
      V.set_buffer_read_only(H.diff_buffer, true)
      V.set_buffer_diff(H.diff_buffer, true)
      if V.buffer_name() ~= H.diff_buffer then open_buffer(H.diff_buffer) end
      H.thread = {}
      show_comments()
      -- The comments already on it, beside their lines.
      H.gh_json({ "api", "repos/{owner}/{repo}/pulls/" .. n .. "/comments", "--paginate" }, function(list)
        H.thread = {}
        for _, c in ipairs(list) do
          local at = c.line or c.original_line
          if at and c.path then
            H.thread[#H.thread + 1] = { path = c.path, side = c.side == "LEFT" and "original" or "modified", line = at, who = login(c.user), body = c.body or "" }
          end
        end
        show_comments()
      end)
      V.message("")
    end, nil, "Getting #" .. n .. "'s changes…")
  end
  if H.pr and H.pr.number == n and H.pr.headRefOid then return with_pr(H.pr) end
  H.gh_json({ "pr", "view", tostring(n), "--json", view_fields }, with_pr, "Asking GitHub…")
end

function H.comment_at(rel, side, at)
  if not H.pr then return end
  ask_text("Comment on " .. rel .. ":" .. at .. ": ", function(text)
    local list = H.pending[H.pr.number] or {}
    list[#list + 1] = { path = rel, side = side, line = at, body = text }
    H.pending[H.pr.number] = list
    show_comments()
    V.message(#list .. " comment" .. (#list == 1 and "" or "s") .. " waiting · S submits the review")
  end)
end

function H.comment_here()
  local row = V.diff_row_here()
  if #row == 0 then return V.message("Put the cursor on a changed or context line") end
  H.comment_at(row[1], row[2], tonumber(row[3]))
end

function H.comment_delete()
  local row = V.diff_row_here()
  local list = H.pr and H.pending[H.pr.number] or {}
  for i = #list, 1, -1 do
    local c = list[i]
    if #row > 0 and c.path == row[1] and c.side == row[2] and c.line == tonumber(row[3]) then table.remove(list, i) end
  end
  show_comments()
end

-- The review's body, as GitHub's API takes it.
function H.review_payload(event, body)
  local comments = {}
  for _, c in ipairs(H.pending[H.pr.number] or {}) do
    comments[#comments + 1] = { path = c.path, line = c.line, side = c.side == "original" and "LEFT" or "RIGHT", body = c.body }
  end
  local payload = { event = event, commit_id = H.pr.headRefOid }
  if body ~= "" then payload.body = body end
  if #comments > 0 then payload.comments = comments end
  return payload
end

function H.submit()
  if not H.pr then return end
  local n = H.pr.number
  local waiting = #(H.pending[n] or {})
  local kinds = { "1. Comment", "2. Approve", "3. Request changes" }
  core.pick("Submit the review of #" .. n .. " (" .. waiting .. " line comments)", kinds, "", function(choice)
    local event = ({ "COMMENT", "APPROVE", "REQUEST_CHANGES" })[core.leading_number(choice) or 1]
    ask_text("Review summary" .. (event == "APPROVE" and " (or RET)" or "") .. ": ", function(body)
      if event ~= "APPROVE" and body == "" and waiting == 0 then return V.message("Nothing to submit") end
      local payload = V.json_encode(H.review_payload(event, body))
      H.gh({ "api", "-X", "POST", "repos/{owner}/{repo}/pulls/" .. n .. "/reviews", "--input", "-" }, function(ok, out, err)
        if not ok then return V.message("gh: " .. G.last_line(err ~= "" and err or out)) end
        H.pending[n] = nil
        V.message("Review submitted on #" .. n)
        H.review(n)
      end, payload, "Submitting…")
    end, true)
  end)
end

-- RET: the line, in this checkout.
function H.diff_visit()
  local row = V.diff_row_here()
  if #row == 0 then return V.message("Not on a change") end
  V.find_file(H.repo_root .. "/" .. row[1])
  V.goto_line_column(tonumber(row[3]), 1)
end

---- An issue

function H.view_issue(n, keep)
  if not keep then
    G.draw(H.issue_buffer, { line("#" .. n .. " · loading…", "git-section") })
    open_buffer(H.issue_buffer)
  end
  H.gh_json({ "issue", "view", tostring(n), "--json", "number,title,body,author,state,labels,comments,url,assignees,createdAt" }, function(issue)
    H.issue = issue
    local out = {}
    out[#out + 1] = pieces({ { "#" .. issue.number .. " ", "git-hash" }, { issue.title or "", "git-section" } })
    local parts = { { (issue.state == "OPEN") and "Open" or "Closed", issue.state == "OPEN" and "green" or "magenta" },
      { " · opened by " .. login(issue.author) .. " " .. H.ago(issue.createdAt), "dim" } }
    for _, l in ipairs(issue.labels or {}) do parts[#parts + 1] = { "  [" .. l.name .. "]", "magenta" } end
    out[#out + 1] = pieces(parts)
    out[#out + 1] = line(issue.url or "", "dim")
    out[#out + 1] = line("")
    body_lines(out, issue.body, "  ")
    out[#out + 1] = line("")
    conversation(out, issue.comments, {})
    out[#out + 1] = line("c comment · x " .. (issue.state == "OPEN" and "close" or "reopen") .. " · b browser · g r refresh · q back", "dim")
    G.draw(H.issue_buffer, out, keep and V.buffer_name() == H.issue_buffer and V.line_number() or nil)
    V.message("")
  end, "Asking GitHub…")
end

function H.close_or_reopen()
  local issue = H.issue
  if not issue then return end
  local verb = issue.state == "OPEN" and "close" or "reopen"
  core.confirm(verb:sub(1, 1):upper() .. verb:sub(2) .. " #" .. issue.number .. "?", function()
    H.gh({ "issue", verb, tostring(issue.number) }, function(ok, _, err)
      if not ok then return V.message("gh: " .. G.last_line(err)) end
      V.message(verb == "close" and "Closed" or "Reopened")
      H.view_issue(issue.number, true)
    end)
  end)
end

---- New pull requests and issues

H.new_kind = nil

local new_help = "# The first line is the title, the rest the description.\n"
  .. "# C-c C-c creates it%s, C-c C-k cancels. Lines starting with # are left out."

function H.new(kind)
  local root = H.root()
  if not root then return end
  H.repo_root, H.new_kind = root, kind
  local text
  if kind == "pr" then
    local branch = V.git_branch(root)
    local base = (G.ask(root, { "rev-parse", "--abbrev-ref", "origin/HEAD" }) or "origin/main"):gsub("%s+$", "")
    local commits = lines_of(G.ask(root, { "log", "--reverse", "--format=%s", base .. "..HEAD" }))
    if #commits == 0 then return V.message(branch .. " has no commits that " .. base .. " doesn't") end
    local body = {}
    if #commits > 1 then for _, c in ipairs(commits) do body[#body + 1] = "- " .. c end end
    text = (#commits == 1 and commits[1] or branch:gsub("[-_/]", " ")) .. "\n\n" .. table.concat(body, "\n")
      .. "\n\n" .. string.format(new_help, " (C-c C-d: as a draft)")
      .. "\n# " .. branch .. " into " .. base:gsub("^origin/", "") .. ", " .. #commits .. " commit" .. (#commits == 1 and "" or "s")
  else
    text = "\n\n" .. string.format(new_help, "")
  end
  open_buffer(H.new_buffer)
  V.set_buffer_read_only(H.new_buffer, false)
  V.set_buffer_text(H.new_buffer, text .. "\n")
  V.goto_line_column(1, 1)
  core.state = "insert"
end

local function new_parts()
  local kept = {}
  for _, l in ipairs(lines_of(V.buffer_string())) do
    if l:sub(1, 1) ~= "#" then kept[#kept + 1] = l end
  end
  local title = (kept[1] or ""):gsub("^%s+", ""):gsub("%s+$", "")
  local body = table.concat(kept, "\n", 2):gsub("^%s+", ""):gsub("%s+$", "")
  return title, body
end

local function create(draft)
  return function()
    local title, body = new_parts()
    if title == "" then return V.message("Write a title on the first line") end
    local kind, root = H.new_kind, H.repo_root
    local args = { kind, "create", "--title", title, "--body-file", "-" }
    if draft and kind == "pr" then args[#args + 1] = "--draft" end
    local function go()
      H.gh(args, function(ok, out, err)
        if not ok then return V.message("gh: " .. G.last_line(err ~= "" and err or out)) end
        core.state = "normal"
        V.set_buffer_text(H.new_buffer, "")
        local url = G.last_line(out)
        local n = tonumber(url:match("/(%d+)$"))
        H.branch_prs = {}
        if n then
          if kind == "pr" then H.view_pr(n) else H.view_issue(n) end
        else
          H.close()
        end
        V.message("Created " .. url)
      end, body, "Creating…")
    end
    core.state = "normal"
    if kind == "pr" and not G.ask(root, { "rev-parse", "--abbrev-ref", "@{upstream}" }) then
      -- Not pushed yet: push the branch (tracking origin's), then create.
      G.async(root, { "push", "-u", "origin", "HEAD" }, "Pushing " .. V.git_branch(root) .. "…", function(ok, _, err)
        if not ok then return V.message("push failed: " .. G.last_line(err)) end
        go()
      end)
    else
      go()
    end
  end
end
H.create = create(false)
H.create_draft = create(true)

function H.new_cancel()
  core.state = "normal"
  V.set_buffer_text(H.new_buffer, "")
  H.close()
  V.message("Not created")
end

---- The branch's pull request, in the status and modeline

-- By "root branch": { pr = {...} or false, at = time asked }.
H.branch_prs = {}

function H.branch_pr_cached()
  local root = G.status_root or H.repo_root
  if not root then return nil end
  local entry = H.branch_prs[root .. " " .. V.git_branch(root)]
  return entry and entry.pr or nil
end

-- Asks GitHub about the branch's pull request (at most once a minute),
-- redrawing the status when it answers.
function H.fetch_branch_pr(root, branch)
  local key = root .. " " .. branch
  local entry = H.branch_prs[key]
  if entry and (entry.busy or os.time() - entry.at < 60) then return end
  H.branch_prs[key] = { pr = entry and entry.pr, at = os.time(), busy = true }
  next_job = next_job + 1
  local tag = tostring(next_job)
  jobs[tag] = function(ok, out)
    local pr = false
    if ok then pr = V.json_decode(out) or false end
    H.branch_prs[key] = { pr = pr, at = os.time() }
    if V.buffer_name() == G.status_buffer and G.status_root == root then G.refresh(true) end
  end
  V.spawn(root, H.program, { "pr", "view", branch, "--json", "number,title,state,isDraft,reviewDecision,statusCheckRollup,url" }, nil, tag)
end

-- The status buffer's line for the branch's pull request (nil: none).
function H.status_line(root, branch)
  if not branch or branch == "" then return nil end
  H.fetch_branch_pr(root, branch)
  local entry = H.branch_prs[root .. " " .. branch]
  local pr = entry and entry.pr
  if not pr then return nil end
  local mark, face, summary = H.checks(pr.statusCheckRollup)
  local parts = { { "PR:       ", "git-label" }, { "#" .. pr.number .. " ", "git-hash" }, { pr.title or "" } }
  if pr.state and pr.state ~= "OPEN" then parts[#parts + 1] = { "  " .. pr.state:lower(), "magenta" } end
  parts[#parts + 1] = { "  " .. mark .. " " .. summary, face }
  local words = review_words[pr.reviewDecision or ""]
  if words then parts[#parts + 1] = { " · " .. words[1], words[2] } end
  return pieces(parts)
end

-- "PR #12 ✓" for the modeline, from what's known (it never asks GitHub).
function H.modeline_segment()
  local root = G.root_here()
  if root == "" then return "" end
  local entry = H.branch_prs[root .. " " .. V.git_branch(root)]
  local pr = entry and entry.pr
  if not pr or pr.state ~= "OPEN" then return "" end
  local mark = H.checks(pr.statusCheckRollup)
  return "PR #" .. pr.number .. (mark ~= " " and (" " .. mark) or "")
end

---- Commands and keys

local commands = {
  ["github-prs"] = function() H.list("pr") end,
  ["github-issues"] = function() H.list("issue") end,
  ["github-pr-new"] = function() H.new("pr") end,
  ["github-issue-new"] = function() H.new("issue") end,
  ["github-open"] = H.open_here,
  ["github-browse"] = H.browse,
  ["github-toggle-all"] = H.toggle_all,
  ["github-refresh"] = H.list_refresh,
  ["github-close"] = H.close,
  ["github-comment"] = H.comment,
  ["github-approve"] = H.approve,
  ["github-request-changes"] = H.request_changes,
  ["github-merge"] = H.merge,
  ["github-checkout"] = H.checkout,
  ["github-review"] = function() H.review() end,
  ["github-review-comment"] = H.comment_here,
  ["github-review-comment-delete"] = H.comment_delete,
  ["github-review-submit"] = H.submit,
  ["github-diff-visit"] = H.diff_visit,
  ["github-issue-close"] = H.close_or_reopen,
  ["github-create"] = H.create,
  ["github-create-draft"] = H.create_draft,
  ["github-create-cancel"] = H.new_cancel,
  ["github-branch-pr"] = function()
    local root = H.root()
    if not root then return end
    H.repo_root = root
    local pr = H.branch_pr_cached()
    if pr then return H.view_pr(pr.number) end
    H.gh_json({ "pr", "view", "--json", "number" }, function(p) H.view_pr(p.number) end, "Asking GitHub…")
  end,
}
for name, fn in pairs(commands) do V.command(name, fn) end

local base = { q = "github-close", b = "github-browse", ["g r"] = "github-refresh" }
local function keymap(extra)
  local map = {}
  for k, v in pairs(base) do map[k] = v end
  for k, v in pairs(extra) do map[k] = v end
  return map
end
core.keymaps.github_prs = keymap({ RET = "github-open", d = "github-review", C = "github-checkout", a = "github-toggle-all", n = "github-pr-new" })
core.keymaps.github_issues = keymap({ RET = "github-open", a = "github-toggle-all", n = "github-issue-new" })
core.keymaps.github_pr = keymap({ d = "github-review", c = "github-comment", a = "github-approve", r = "github-request-changes", m = "github-merge", C = "github-checkout" })
core.keymaps.github_diff = keymap({ c = "github-review-comment", C = "github-review-comment-delete", S = "github-review-submit", RET = "github-diff-visit" })
core.keymaps.github_issue = keymap({ c = "github-comment", x = "github-issue-close" })
core.keymaps.github_new = { ["C-c C-c"] = "github-create", ["C-c C-d"] = "github-create-draft", ["C-c C-k"] = "github-create-cancel", ["Z Z"] = "github-create" }

for _, e in ipairs({
  { H.prs_buffer, "github_prs" }, { H.pr_buffer, "github_pr" }, { H.diff_buffer, "github_diff" },
  { H.issues_buffer, "github_issues" }, { H.issue_buffer, "github_issue" }, { H.new_buffer, "github_new", "github_new" },
}) do
  core.buffer_keymaps[#core.buffer_keymaps + 1] = e
end

for key, cmd in pairs({
  ["SPC g p"] = "github-prs", ["SPC g i"] = "github-issues", ["SPC g n"] = "github-pr-new",
  ["SPC g I"] = "github-issue-new", ["SPC g h"] = "github-branch-pr",
}) do
  core.define_key("normal", key, cmd)
end

-- In *git-status*: @ p / @ i / @ n / @ v.
local status = core.keymaps.git_status
status["@ p"], status["@ i"], status["@ n"], status["@ v"] = "github-prs", "github-issues", "github-pr-new", "github-branch-pr"
status["@"] = G.prefix("@")
