#!/usr/bin/env python3
"""Answer native and late questions through an isolated Deck and phone."""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from chat_ui_check import Device
from chat_ui_check import assert_private_guest
from chat_ui_check import m
from chat_ui_check import touch_target

CODEX = r"""import datetime,json,os,sys,termios,tty,uuid
fd=sys.stdin.fileno()
tty.setraw(fd)
root=os.environ["CODEX_HOME"]
conversation=str(uuid.uuid4())
now=datetime.datetime.now(datetime.timezone.utc)
folder=os.path.join(root,"sessions",now.strftime("%Y/%m/%d"))
os.makedirs(folder,exist_ok=True)
log=os.path.join(folder,"rollout-"+now.strftime("%Y-%m-%dT%H-%M-%S-")+conversation+".jsonl")
def record(value):
 value["timestamp"]=datetime.datetime.now(datetime.timezone.utc).isoformat()
 with open(log,"a") as output: output.write(json.dumps(value)+"\n")
def item(value):
 value={"type":"event_msg","payload":{"type":"item_completed","item":value},"timestamp":"2026-11-02T20:00:00Z"}
 with open(log,"a") as output: output.write(json.dumps(value)+"\n")
record({"type":"session_meta","payload":{"id":conversation,"cwd":os.getcwd(),"source":"cli","thread_source":"user"}})
item({"type":"UserMessage","id":"start","content":[{"type":"Text","text":"Native question fixture"}]})
record({"type":"event_msg","payload":{"type":"user_message","message":"Native question fixture"}})
for index in range(3):
 item({"type":"Extension","id":f"long-tool-{index}","kind":f"mcp__fixture_{index}__"+"very_long_tool_namespace_"*4})
titles=["Should `:cd` start a new chat?", "Keep **the current conversation**?", "Open [Resume](https://example.com/resume)?", "Which late answer?"]
visible_titles=["Should :cd start a new chat?", "Keep the current conversation?", "Open Resume?", "Which late answer?"]
questions=[{"title":title,"options":[f"Q{i+1} first",f"Q{i+1} second",f"Q{i+1} third"]} for i,title in enumerate(titles)]
item({"type":"AgentMessage","id":"question-set","content":[{"type":"Text","text":"Choose an answer for each question."}],"delivery":"async","questions":questions})
pending=[0,1,2]
position=0
expanded=False
trace=os.path.join(os.getcwd(),"answers.jsonl")
def show():
 rows=["• Working (4s • esc to interrupt)","", "• Queued follow-up inputs", ""]
 if expanded and pending:
  current=pending[position]
  if len(pending)>1: rows += [f"{position+1} of {len(pending)}"]
  rows += [visible_titles[current],""]
  rows += [("›" if i==0 else " ")+f" {i+1}. {option}" for i,option in enumerate(questions[current]["options"])]
  navigation="prev question" if position>0 else "main prompt"
  if position+1<len(pending): navigation += " · ⇧← next question"
  rows += ["  4. Other","", "enter submit   ⌃] skip   ⇧→ "+navigation]
 elif pending:
  rows += ["Queued follow-up inputs",f"  ? {len(pending)} questions","    shift+← to answer","> "]
 else: rows += ["All native questions answered","> "]
 os.write(1,("\x1b[2J\x1b[H"+"\r\n".join(rows)).encode())
os.write(1,b"\x1b[?2004h")
show()
buffer=""
while True:
 data=os.read(fd,1024).decode(errors="replace")
 if not data: break
 if "\x1b[1;2D" in data:
  if expanded: position=min(position+1,len(pending)-1)
  expanded=True
  show()
 elif "\x1b[1;2C" in data and expanded:
  if position>0: position-=1
  else: expanded=False
  show()
 elif expanded and data in ("1","2","3"):
  current=pending.pop(position)
  choice=int(data)-1
  answer=questions[current]["options"][choice]
  with open(trace,"a") as output: output.write(json.dumps({"question":current,"choice":choice})+"\n")
  reply="<send_user_message_question_reply>"+json.dumps([{"answer":answer,"question":questions[current]["title"],"questionItemId":json.dumps(["request_user_input_async","question-set",current])}])+"</send_user_message_question_reply>"
  item({"type":"UserMessage","id":str(uuid.uuid4()),"content":[{"type":"Text","text":reply}]})
  position=min(position,len(pending)-1)
  show()
 elif not pending:
  buffer+=data
  if "\r" in buffer:
   text=buffer.replace("\x1b[200~","").replace("\x1b[201~","").strip()
   marker="<send_user_message_question_reply>"
   if marker in text:
    answers=json.loads(text.split(marker,1)[1].split("</send_user_message_question_reply>",1)[0])
    reply=answers[0]
    current=json.loads(reply["questionItemId"])[2]
    choice=questions[current]["options"].index(reply["answer"])
    with open(trace,"a") as output: output.write(json.dumps({"question":current,"choice":choice})+"\n")
    item({"type":"UserMessage","id":str(uuid.uuid4()),"content":[{"type":"Text","text":text}]})
   buffer=""
   show()
"""


def choice_buttons(nodes, label):
    buttons = []
    for node in nodes:
        if node["text"] != label:
            continue
        try:
            buttons.append(touch_target(nodes, node))
        except ValueError:
            # The logged answer bubble repeats the option's text, but has no
            # clickable button ancestor. Only question controls count.
            continue
    return buttons


def main():
    assert_private_guest()
    m.guard()
    root = Path(tempfile.mkdtemp(prefix="question-choice-")).resolve()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    shots.mkdir(parents=True, exist_ok=True)
    m.STAND_IN = CODEX
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), root)
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    summary = {}
    try:
        base_settle = m.settle

        def settle(current):
            base_settle(current)
            current.shell("pm disable-user --user 0 com.google.android.tts", check=False)
            current.shell(
                "pm disable-user --user 0 com.google.android.googlequicksearchbox", check=False
            )

        m.settle = settle
        m.stats = lambda *_: None
        deck.start()
        m.journey(device, deck, Path(os.environ["CEPHEUS_APK"]), summary, stats_only=True)
        deck.ctl("new", "codex", "--name", "Question choices")
        m.wait_for(
            "fixture conversation linked",
            60,
            lambda: any(s.get("conversation") for s in deck.sessions()),
        )
        device.tap(device.wait("Codex question session", text="Question choices", timeout=30))
        device.wait("question choices loaded", text="Q4 second", timeout=60)
        device.screenshot("questions-loaded")
        trace = root / "work/answers.jsonl"
        expected = []
        previous_question = 3
        for question, choice in [(2, 2), (0, 0), (1, 1), (3, 1)]:
            label = f"Q{question + 1} " + ["first", "second", "third"][choice]
            for _ in range(8):
                nodes = device.nodes()
                composer_top = min(
                    n["bounds"][1] for n in nodes if n["class"] == "android.widget.EditText"
                )
                found = choice_buttons(nodes, label)
                found = [
                    n for n in found if 260 <= n["bounds"][1] and n["bounds"][3] < composer_top
                ]
                if found:
                    previous_bounds = found[0]["bounds"]
                    nodes = device.nodes()
                    found = choice_buttons(nodes, label)
                    found = [
                        n for n in found if 260 <= n["bounds"][1] and n["bounds"][3] < composer_top
                    ]
                    if found and found[0]["bounds"] == previous_bounds:
                        break
                device.shell(
                    "input swipe 360 400 360 1100 300"
                    if question < previous_question
                    else "input swipe 360 1100 360 400 300"
                )
            else:
                raise m.CheckFailed("Missing answer button: " + label)
            if not found[0]["enabled"]:
                raise m.CheckFailed("Unanswered choices are disabled: " + label)
            device.screenshot(f"question-{question + 1}-before-tap")
            device.tap(found[0])
            previous_question = question
            expected.append({"question": question, "choice": choice})
            m.wait_for(
                "matching answer accepted on the first tap",
                15,
                lambda: trace.exists() and len(trace.read_text().splitlines()) == len(expected),
            )
            actual = [json.loads(line) for line in trace.read_text().splitlines()]
            if actual != expected:
                raise m.CheckFailed(f"Wrong native answers: {actual}")
            device.screenshot(f"question-{question + 1}-answered")
        session = next(s["id"] for s in deck.sessions() if s["tool"] == "codex")
        m.wait_for(
            "native question queue cleared",
            10,
            lambda: "All native questions answered" in deck.screen(session),
        )
        summary["three_formatted_native_choices_and_one_late_answer_accepted"] = True
        device.shell("am force-stop " + m.PACKAGE)
        device.shell("am start -n " + m.PACKAGE + "/" + m.ACTIVITY)
        # Startup returns Home; reopening the same session reads authoritative
        # answered state rather than the composable's previous selection.
        found = device.wait("restored session", text="Question choices", timeout=30)
        device.tap(found)
        for question in reversed(range(4)):
            label = f"Q{question + 1} second"
            for _ in range(16):
                nodes = device.nodes()
                found = choice_buttons(nodes, label)
                if found:
                    break
                device.shell("input swipe 360 400 360 1100 300")
            else:
                raise m.CheckFailed("Missing answered question after reopening")
            if found[0]["enabled"]:
                raise m.CheckFailed("Answered choices became active after reopening")
            device.screenshot(f"question-{question + 1}-after-reopen")
        summary["answered_choices_stay_inactive_after_app_restart"] = True
        for index in reversed(range(3)):
            label = f"mcp__fixture_{index}__" + "very_long_tool_namespace_" * 4
            for _ in range(16):
                nodes = device.nodes()
                composer_top = min(
                    n["bounds"][1] for n in nodes if n["class"] == "android.widget.EditText"
                )
                visible = [
                    n for n in nodes if 260 <= n["bounds"][1] and n["bounds"][3] < composer_top
                ]
                found = [n for n in visible if n["text"] == label]
                if found:
                    break
                folded = [n for n in visible if n["desc"] == "Show steps"]
                if folded:
                    # A run of calls is one line until it is opened (owner, 2026-11-03).
                    device.tap(folded[0])
                    continue
                device.shell("input swipe 360 400 360 1100 300")
            else:
                raise m.CheckFailed("Missing long tool card")
            target = found[0]
            arrows = [
                n
                for n in device.nodes()
                if n["desc"] == "Show details"
                and abs(sum(n["bounds"][1::2]) - sum(target["bounds"][1::2])) < 20
            ]
            if not arrows:
                raise m.CheckFailed("Long tool name hid its arrow")
            device.tap(arrows[0])
            device.wait("expanded long tool", desc="Hide details")
            device.screenshot(f"long-tool-{index}-expanded")
            device.tap(device.find(desc="Hide details")[0])
        summary["three_long_tool_arrows_visible_and_tappable"] = True
        for _ in range(8):
            if device.find(contains="Nov 2, 2026"):
                break
            device.shell("input swipe 360 400 360 1100 300")
        else:
            raise m.CheckFailed("The original message send date is missing")
        device.screenshot("question-message-timestamp")

    except Exception as error:
        summary["error"] = str(error)
        (shots / "fixture-sessions.json").write_text(json.dumps(deck.sessions(), indent=2))
        shutil.copyfile(root / "service.log", shots / "fixture-service.log")
        device.screenshot("failure")
        (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
        raise
    finally:
        deck.stop()
        (shots / "question-summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
