-- Comments on changes, sent to the agent to revise by.
--
-- The agent's changes wait for review (ga accepts, gx rejects; SPC a r lists
-- them in *agent-review*, drawn with gpui-kit's Diff). On any line of them,
-- or of any file:
--   gc        comment on this line (shown under it in the review, and
--             marked in the file)
--   gC        delete the comments on this line
--   SPC a S   send all comments to the agent, asking it to revise its
--             changes by them (ACP or CLI agent alike)
-- In *agent-review*, c and C do what gc and gC do, S sends. The Diff's +
-- button beside a hovered line comments on it with the mouse.

local function comment()
  local existing = vmac.review_comment_text_here()
  if existing ~= "" then
    vmac.message("This line has: " .. existing .. "  (gC deletes it)")
  end
  vmac.prompt("Comment: ", function(text)
    vmac.review_comment_add(text)
  end)
end

vmac.command("review-comment", comment)
vmac.command("review-comment-delete-here", function() vmac.review_comment_delete() end)
vmac.command("review-send", function() vmac.review_comments_send() end)

vmac.map("normal", "g c", "review-comment")
vmac.map("normal", "g C", "review-comment-delete-here")
vmac.map("normal", "SPC a S", "review-send")
vmac.map("review", "c", "review-comment")
vmac.map("review", "C", "review-comment-delete-here")
vmac.map("review", "S", "review-send")

-- The Diff's comment button: a comment on the line it was beside (in a
-- pull request's diff, a comment for its review).
vmac.hooks.review_comment_at = function(rel, side, line, buffer)
  if core.github and buffer == core.github.diff_buffer then
    return core.github.comment_at(rel, side, tonumber(line))
  end
  vmac.prompt("Comment on " .. rel .. ":" .. line .. ": ", function(text)
    vmac.review_comment_at_line(rel, side == "modified", tonumber(line), text)
  end)
end
