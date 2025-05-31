# Task handling

Ideally:

* If old task is likely to be `is_incomplete` then we can't filter its results. So it should be cancelled

* If old task is likely to be complete, then let it run.

For now just being optimistic about tasks instead of cancelling them.


On edit

* Cancel all tasks that aren't usable. Similar to logic for whether to keep doing completion on selection change - based on matching offset position and query prefix.

* If can reuse completion:

  - Run filtering on it.

  - Completions menu keeps track of its filter task. Sets cancel bool to true on drop.

  - What do we show while filtering is happening? UI perception / action race here. Let's try just showing the old state. Filter should be very fast.

* If can't reuse:

  - Run new completion task.

  -

* On completion task completion:

  - If menu

  - If `is_incomplete: false` then cancel all completion tasks that are suffixes.

# Is same completion logic

* Does the cursor position match?

* Are there query chars to the left?

struct CompletionsTask {
    pub position: Anchor,
    pub query: SharedString,
    pub task: Task<()>,
}

# Task management

Ahah, instead of tracking position with the tasks, can just stick them in the
menu.

So, editor has `CompletionsMenu`. `CompletionsMenu` has `VisibleCompletions`


# Sketch

`CompletionsMenu::new` creates a completions context for a particular position.

`Editor:open_completons_menu`

    * If there is no menu

        - `CompletionsMenu::new`
        - `CompletionsMenu::query_completions`

    * If there is one

        - `CompletionsMenu::query_completions`


todo! "show word completions" simply doesn't work

todo! fewer max results of filter?

# Handling multiple sources

* Now a vec of completions

*
