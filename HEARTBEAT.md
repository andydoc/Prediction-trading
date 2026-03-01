# HEARTBEAT.md

## Autonomous Agent Workflow Instructions

This document defines the core operational loop for AI agents working in this workspace.

---

## 🔄 Primary Loop

### 1. SCAN for New Work
**Location:** `C:\Users\andyd\ai-workspace\tasks\`

**Actions:**
- Check for any `.md`, `.txt`, or other task files in `/tasks`
- Identify files marked as:
  - `[NEW]` in filename or content
  - `Status: Pending` or `Status: New` in task files
  - Files without `Status: Complete` or `Status: Done`
- Prioritize by:
  1. Files with `Priority: High` or `Priority: Urgent`
  2. Files with explicit deadlines
  3. Oldest files first (by creation date)

### 2. EXECUTE Assignments
**Process:**
- Read task file completely
- Understand requirements and objectives
- Break down complex tasks into steps
- Execute each step methodically
- Document progress and decisions
- Test/verify outputs when applicable

**Best Practices:**
- Follow task instructions precisely
- Use available tools and resources
- Make reasonable assumptions when details are unclear
- Document all assumptions made
- Show your work and reasoning

### 3. ASK When Stuck
**Location:** `C:\Users\andyd\ai-workspace\brain-inbox\`

**When to write questions:**
- Requirements are ambiguous or contradictory
- Missing critical information to proceed
- Multiple valid approaches exist (need direction)
- Unsure about scope or boundaries
- Technical blocker or limitation encountered

**Question Format:**
```markdown
# QUESTION: [Brief title]

**Date:** YYYY-MM-DD
**Related Task:** [filename or task name]
**Urgency:** [Low/Medium/High]

## Context
[Brief explanation of what you're working on]

## Question
[Clear, specific question]

## Why I'm Stuck
[Explanation of the blocker]

## Possible Options (if applicable)
1. Option A: [description]
2. Option B: [description]

## What I Need to Proceed
[Specific information or decision needed]
```

**Filename Convention:**
`YYYY-MM-DD-question-about-[task-name].md`

### 4. DELIVER Completed Work
**Location:** `C:\Users\andyd\ai-workspace\results\`

**What to deliver:**
- All code, documents, or outputs produced
- Summary of what was accomplished
- Any relevant notes or documentation
- Links to related resources if applicable

**Result Format:**
```markdown
# [Task Name] - COMPLETED

**Completed:** YYYY-MM-DD
**Original Task:** [link to task file or name]
**Time Spent:** [if tracked]

## Summary
[Brief description of what was accomplished]

## Deliverables
- [List of files created]
- [List of outputs generated]
- [Any other artifacts]

## Notes
- [Important decisions made]
- [Challenges encountered and solved]
- [Future improvements or follow-ups]

## Files
[Actual code/content or links to files]
```

**Filename Convention:**
`YYYY-MM-DD-[task-name]-COMPLETE.md`

**Folder Organization:**
```
results/
├── 2026-02/
│   ├── completed-task-1/
│   └── completed-task-2/
└── code-snippets/
```

---

## 🔍 Health Checks

### Before Starting Work
- [ ] All required tools and resources are available
- [ ] Workspace directories are accessible
- [ ] Previous tasks don't have blocking dependencies

### During Execution
- [ ] Progress is being made (not stuck in loops)
- [ ] Assumptions are documented
- [ ] Quality standards are maintained

### After Completion
- [ ] Task marked as complete in original file
- [ ] All deliverables moved to `/results`
- [ ] No loose ends or temporary files in `/tasks`
- [ ] Questions file created if needed

---

## 📊 Status Updates

### Task Status Markers
Update the original task file with status:
```markdown
**Status:** In Progress  
**Started:** YYYY-MM-DD HH:MM
**Last Updated:** YYYY-MM-DD HH:MM
```

When complete:
```markdown
**Status:** Complete  
**Completed:** YYYY-MM-DD HH:MM
**Result Location:** results/YYYY-MM-DD-[task-name]-COMPLETE.md
```

---

## 🚨 Error Handling

### If Something Goes Wrong
1. Document the error clearly
2. Save current progress to `/results` with `[PARTIAL]` tag
3. Write detailed question to `/brain-inbox`
4. Mark task as `Status: Blocked` with explanation

### Error Report Format
```markdown
# ERROR REPORT: [Task Name]

**Date:** YYYY-MM-DD
**Error Type:** [Technical/Requirements/Resource]

## What Happened
[Clear description of the error]

## Current State
[What's been completed, what's left]

## Attempted Solutions
[What you tried to fix it]

## Help Needed
[What's needed to resolve]
```

---

## ⚙️ Configuration

### Model Settings
- **Model:** qwen2.5-coder:32b
- **Context:** 128k tokens
- **Mode:** Local (Ollama)

### Workspace Paths
- **Tasks:** `C:\Users\andyd\ai-workspace\tasks`
- **Results:** `C:\Users\andyd\ai-workspace\results`
- **Brain Inbox:** `C:\Users\andyd\ai-workspace\brain-inbox`

---

## 🎯 Success Criteria

A heartbeat cycle is successful when:
- [x] All new tasks were identified
- [x] At least one task progressed or completed
- [x] Clear questions written for blockers
- [x] Completed work properly documented
- [x] Workspace is clean and organized

---

## 📝 Operational Notes

### Frequency
Run this heartbeat loop:
- **Manual trigger:** When user initiates
- **Automated:** If configured with scheduler/cron
- **On-demand:** When new tasks appear

### Autonomy Level
**Semi-Autonomous:** 
- Execute clear, well-defined tasks automatically
- Ask questions for ambiguous situations
- Wait for human approval on high-impact decisions

### Communication
- Be clear and concise in all documentation
- Use consistent formatting
- Always provide context
- Make it easy for humans to understand your work

---

**Version:** 1.0  
**Last Updated:** 2026-02-10  
**Purpose:** Define autonomous agent workflow for ai-workspace
