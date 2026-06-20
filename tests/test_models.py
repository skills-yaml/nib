"""Basic model tests."""

from nib.core.models import Priority, Task, TaskStatus


def test_task_defaults() -> None:
    task = Task(id="t1", title="Implement workload persistence")
    assert task.status == TaskStatus.TODO
    assert task.priority == Priority.MEDIUM
    assert task.tags == []
