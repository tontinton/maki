-- The enums the todo_write tool's schema registers, and the statuses by name.

local M = {
  statuses = { "pending", "in_progress", "completed", "cancelled" },
  priorities = { "high", "medium", "low" },
  pending = "pending",
  in_progress = "in_progress",
  completed = "completed",
  cancelled = "cancelled",
}

return M
