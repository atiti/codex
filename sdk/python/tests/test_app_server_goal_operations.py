from app_server_harness import (
    AppServerHarness,
    ev_assistant_message,
    ev_completed,
    ev_function_call,
    ev_response_created,
    sse,
)
from app_server_helpers import agent_message_texts

from openai_codex import Codex
from openai_codex._goal import _GoalNotificationStream
from openai_codex._run import _collect_turn_result
from openai_codex.generated.notification_registry import notification_turn_id
from openai_codex.generated.v2_all import ThreadGoalGetResponse, ThreadGoalStatus, TurnStatus


def test_private_goal_operation_coalesces_runtime_continuations(tmp_path) -> None:
    """The private engine should expose automatic continuations as one turn."""
    with AppServerHarness(tmp_path) as harness:
        harness.responses.enqueue_assistant_message(
            "Initial pass complete.",
            response_id="goal-initial",
        )
        harness.responses.enqueue_sse(
            sse(
                [
                    ev_response_created("goal-complete-tool"),
                    ev_function_call(
                        "call-goal-complete",
                        "update_goal",
                        '{"status":"complete"}',
                    ),
                    ev_completed("goal-complete-tool"),
                ]
            )
        )
        harness.responses.enqueue_assistant_message(
            '{"decision":"complete","reason":"Benchmark coverage verified."}',
            response_id="goal-review",
        )
        harness.responses.enqueue_sse(
            sse(
                [
                    ev_response_created("goal-final"),
                    ev_assistant_message("msg-goal-final", "Goal complete."),
                    ev_completed("goal-final"),
                ]
            )
        )

        with Codex(config=harness.app_server_config()) as codex:
            thread = codex.thread_start()
            state, turn_id = codex._client.start_goal_operation(  # noqa: SLF001
                thread.id,
                "Improve benchmark coverage",
            )
            stream = _GoalNotificationStream(
                state,
                state.next_notification,
                lambda: codex._client.unregister_goal_operation(state),  # noqa: SLF001
                lambda: codex._client.cancel_goal_operation(state),  # noqa: SLF001
            )
            events = list(stream)
            result = _collect_turn_result(iter(events), turn_id=turn_id)
            routes = codex._client._router._goal_operations.copy()  # noqa: SLF001
            requests = harness.responses.wait_for_requests(4)

    lifecycle = [event.method for event in events if event.method.startswith("turn/")]
    routed_ids = [
        routed_id
        for event in events
        if (routed_id := notification_turn_id(event.payload)) is not None
    ]
    assert {
        "lifecycle": lifecycle,
        "routed_ids": routed_ids,
        "result": (result.id, result.status, result.final_response),
        "messages": agent_message_texts(events),
        "request_count": len(requests),
        "objective_reached_model": (
            "<objective>\nImprove benchmark coverage\n</objective>"
            in "\n".join(requests[0].message_input_texts("user"))
        ),
        "routes_after_completion": routes,
    } == {
        "lifecycle": ["turn/started", "turn/completed"],
        "routed_ids": [turn_id] * len(routed_ids),
        "result": (turn_id, TurnStatus.completed, "Goal complete."),
        "messages": [
            "Initial pass complete.",
            "Review agent · checking completion (attempt 1).\n"
            "Independently verifying the goal before it can be marked complete.",
            "Review agent · complete (attempt 1).\n"
            "Benchmark coverage verified.\nCompletion check passed.",
            "Goal complete.",
        ],
        "request_count": 4,
        "objective_reached_model": True,
        "routes_after_completion": {},
    }


def test_private_goal_operation_retries_after_continue_review(tmp_path) -> None:
    """A continue verdict keeps the goal active and sends its reason back to the model."""
    with AppServerHarness(tmp_path) as harness:
        harness.responses.enqueue_assistant_message(
            "Initial pass complete.",
            response_id="goal-initial",
        )
        harness.responses.enqueue_sse(
            sse(
                [
                    ev_response_created("goal-first-complete-tool"),
                    ev_function_call(
                        "call-goal-complete-first",
                        "update_goal",
                        '{"status":"complete"}',
                    ),
                    ev_completed("goal-first-complete-tool"),
                ]
            )
        )
        harness.responses.enqueue_assistant_message(
            '{"decision":"continue","reason":"No release evidence."}',
            response_id="goal-review-continue",
        )
        harness.responses.enqueue_sse(
            sse(
                [
                    ev_response_created("goal-second-complete-tool"),
                    ev_function_call(
                        "call-goal-complete-second",
                        "update_goal",
                        '{"status":"complete"}',
                    ),
                    ev_completed("goal-second-complete-tool"),
                ]
            )
        )
        harness.responses.enqueue_assistant_message(
            '{"decision":"complete","reason":"Release evidence is verified."}',
            response_id="goal-review-complete",
        )
        harness.responses.enqueue_sse(
            sse(
                [
                    ev_response_created("goal-final"),
                    ev_assistant_message("msg-goal-final", "Goal complete."),
                    ev_completed("goal-final"),
                ]
            )
        )

        with Codex(config=harness.app_server_config()) as codex:
            thread = codex.thread_start()
            state, turn_id = codex._client.start_goal_operation(  # noqa: SLF001
                thread.id,
                "Verify release evidence",
            )
            stream = _GoalNotificationStream(
                state,
                state.next_notification,
                lambda: codex._client.unregister_goal_operation(state),  # noqa: SLF001
                lambda: codex._client.cancel_goal_operation(state),  # noqa: SLF001
            )
            events = list(stream)
            result = _collect_turn_result(iter(events), turn_id=turn_id)
            requests = harness.responses.wait_for_requests(6)
            goal = codex._client.request(  # noqa: SLF001
                "thread/goal/get",
                {"threadId": thread.id},
                response_model=ThreadGoalGetResponse,
            ).goal

    messages = agent_message_texts(events)
    continue_message = (
        "Review agent · continue (attempt 1).\n"
        "No release evidence.\nGoal remains active; work continues."
    )
    assert result.status == TurnStatus.completed
    assert result.final_response == "Goal complete."
    assert goal is not None
    assert goal.status == ThreadGoalStatus.complete
    assert continue_message in messages
    assert messages.index(continue_message) < messages.index(
        "Review agent · checking completion (attempt 2).\n"
        "Independently verifying the goal before it can be marked complete."
    )
    assert any(
        item.get("type") == "function_call_output"
        and item.get("call_id") == "call-goal-complete-first"
        and "No release evidence." in str(item.get("output"))
        for item in requests[3].input()
    )
    assert messages.index(
        "Review agent · complete (attempt 2).\n"
        "Release evidence is verified.\nCompletion check passed."
    ) < messages.index("Goal complete.")
