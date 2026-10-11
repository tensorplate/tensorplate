"""The bounded buffer between a session's reader and its caller."""

from __future__ import annotations

import asyncio
import math
from collections import deque
from typing import Generic, TypeVar

T = TypeVar("T")


class Signal:
    """Wakes every task that waits for a change of state."""

    def __init__(self) -> None:
        self._event = asyncio.Event()

    def notify(self) -> None:
        self._event.set()
        self._event = asyncio.Event()

    async def wait(self) -> None:
        await self._event.wait()


class EventBuffer(Generic[T]):
    """Events read from a call and not yet taken, in the order read, under two bounds.

    Audio counts against one bound by its bytes, and every other event
    against the other by its size on the wire. ``put`` waits while the
    event's bound is reached, which is what stops the session reading from
    the transport. An event is always admitted when nothing is held against
    its bound, whatever its size.
    """

    def __init__(self, capacity: int) -> None:
        # Both are indexed by whether an event is audio.
        self._capacity: list[float] = [capacity, 0]
        self._used = [0, 0]
        self._items: deque[tuple[T, int, bool]] = deque()
        self._ended = False
        self._changed = Signal()

    @property
    def used(self) -> int:
        return self._used[False]

    @property
    def audio_used(self) -> int:
        return self._used[True]

    def bound_audio(self, capacity: int) -> None:
        """Hold at most `capacity` bytes of audio."""
        self._capacity[True] = capacity

    async def put(
        self, item: T, size: int, patience: float | None = None, *, audio: bool = False
    ) -> None:
        """Admit `item` once it fits its bound; an ended buffer drops it.

        Raises ``asyncio.TimeoutError`` when `patience` seconds pass with
        that bound reached and nothing taken from the buffer.
        """
        while (
            self._used[audio]
            and self._used[audio] + size > self._capacity[audio]
            and not self._ended
        ):
            await asyncio.wait_for(self._changed.wait(), patience)
        if not self._ended:
            self._items.append((item, size, audio))
            self._used[audio] += size
            self._changed.notify()

    async def get(self) -> T | None:
        """The next event, or ``None`` once the buffer has ended and holds nothing."""
        while not self._items:
            if self._ended:
                return None
            await self._changed.wait()
        item, size, audio = self._items.popleft()
        self._used[audio] -= size
        self._changed.notify()
        return item

    def unbound(self) -> None:
        """Admit whatever still comes: the call has ended, so the transport holds little more."""
        self._capacity = [math.inf, math.inf]
        self._changed.notify()

    def end(self, *, discard: bool = False) -> None:
        """Admit nothing more; with ``discard``, also drop what is held."""
        self._ended = True
        if discard:
            self._items.clear()
            self._used = [0, 0]
        self._changed.notify()
