"""TC-01 Page Object: the host opens a voice party room, adds a local song and plays it.

Hafa is a Flutter app, so all locators use content-desc only.
Constants use the format (text, fallback coordinate). The coordinate is None when no fallback is needed.
"""
import re


def _secs(mmss: str) -> int:
    m, s = mmss.split(":")
    return int(m) * 60 + int(s)


class HomePage:
    MINE_TAB = ("Mine\nMine", None)
    PARTY_TAB = ("Party\nParty", None)
    PARTY_READY = "Recommend"
    # Plus icon in the top-right corner of the Party tab. It has no text, so it is tapped by coordinate.
    CREATE_ROOM_PLUS_XY = (665, 121)

    def __init__(self, ui):
        self.ui = ui

    def open_app(self):
        self.ui.restart_app()
        # Close the daily check-in or guide popup if one appears (close only, do not check in).
        self.ui.close_popups()

    def open_mine(self):
        text, xy = self.MINE_TAB
        self.ui.tap(text, xy=xy)
        self.ui.wait_text(MinePage.READY)

    def open_party(self):
        text, xy = self.PARTY_TAB
        self.ui.tap(text, xy=xy)
        self.ui.wait_text(self.PARTY_READY)

    def tap_create_room(self):
        # Plus icon in the top-right corner, a fixed-position icon with no text.
        self.ui.tap_xy(*self.CREATE_ROOM_PLUS_XY)
        self.ui.wait_text(VoicePartyPage.START_PARTY[0])


class MinePage:
    READY = "Following"
    # The nickname label looks like "Ybpj4q / ID: 1081203".
    NICK_PATTERN = r"^.+ / ID: ?\d+$"

    def __init__(self, ui):
        self.ui = ui

    def read_nickname(self) -> str:
        label = self.ui.read(self.NICK_PATTERN)
        assert label, "Mine 页未读到「昵称 / ID」信息"
        return label.split(" / ID")[0].strip()


class VoicePartyPage:
    # The room name input box sits right after "Change". Its content-desc is its prefilled old value.
    ROOM_NAME_PREV = "Change"
    ROOM_NAME_XY = (360, 538)
    START_PARTY = ("Start Party", None)
    MIC_PERMISSION = "Microphone Permission Required"
    MIC_ENABLE = ("Enable", None)
    SYS_ALLOW_WHILE_USING = ["仅在使用该应用时允许", "While using the app"]

    def __init__(self, ui):
        self.ui = ui

    def set_room_name(self, name: str):
        self.ui.type_after(self.ROOM_NAME_PREV, name, xy=self.ROOM_NAME_XY)
        self.ui.wait_text(name, exact=True)

    def start_party(self):
        text, xy = self.START_PARTY
        self.ui.tap(text, xy=xy)
        self.ui.wait_any([RoomPage.READY, self.MIC_PERMISSION], timeout=20)

    def handle_mic_permission(self):
        """Handle the microphone permission prompt only if it appears."""
        if not self.ui.has(self.MIC_PERMISSION):
            return
        text, xy = self.MIC_ENABLE
        self.ui.tap(text, xy=xy)
        try:
            hit = self.ui.wait_any(self.SYS_ALLOW_WHILE_USING + [RoomPage.READY], timeout=8)
        except AssertionError:
            return
        if hit in self.SYS_ALLOW_WHILE_USING:
            self.ui.tap(hit)


class RoomPage:
    READY = "Announcement"
    # The More grid icon at the bottom of the room. It has no text, so it is tapped by coordinate.
    MORE_XY = (295, 1527)
    ROOM_TOOLS = "Room Tools"
    MUSIC = ("Music", None)

    def __init__(self, ui):
        self.ui = ui

    def wait_entered(self, room_name: str):
        self.ui.wait_text(self.READY, timeout=20)
        # The voice debug overlay may cover the page, so close it first.
        self.ui.dismiss_debug_overlay()
        self.ui.wait_text(f"{room_name} / ID:")

    def open_music(self):
        self.ui.dismiss_debug_overlay()
        # The More grid icon at the bottom of the room.
        self.ui.tap_xy(*self.MORE_XY)
        self.ui.wait_text(self.ROOM_TOOLS)
        text, xy = self.MUSIC
        self.ui.tap(text, xy=xy)
        self.ui.wait_text(MusicPage.TITLE_PREFIX)


class MusicPage:
    TITLE_PREFIX = "Music("
    TITLE_EMPTY = "Music(0/50)"
    TITLE_ONE = "Music(1/50)"
    ADD_MUSIC = ("Add Music", None)
    ADDED_TOAST = "Music added successfully"
    SYS_ALLOW = ["允许", "Allow"]

    def __init__(self, ui):
        self.ui = ui

    def title(self) -> str | None:
        return self.ui.read(r"^Music\(\d+/50\)$")

    def tap_add_music(self):
        text, xy = self.ADD_MUSIC
        self.ui.tap(text, xy=xy)
        hit = self.ui.wait_any([LocalLibraryPage.READY] + self.SYS_ALLOW, timeout=10)
        # Tap allow only if the music access permission prompt appears.
        if hit in self.SYS_ALLOW:
            self.ui.tap(hit)
            self.ui.wait_text(LocalLibraryPage.READY)

    def song_row_label(self, song: str) -> str | None:
        for label in self.ui.labels():
            if label == song or label.startswith(f"{song} / "):
                return label
        return None

    def play_song(self, song: str):
        # The list order is not fixed, so locate the row by song name.
        self.ui.tap(song, exact=False, scroll=True)
        self.ui.wait_text(PlayerPage.CURRENT_PLAYER, timeout=10)


class LocalLibraryPage:
    READY = "Local Music Library"

    def __init__(self, ui):
        self.ui = ui

    def select_song(self, song: str):
        # The library order differs on every run, so locate by song name and scroll to find it.
        self.ui.tap(song, exact=False, scroll=True)
        self.ui.wait_text("Add(1)", exact=True)

    def confirm_add(self, count: int = 1):
        self.ui.tap(f"Add({count})")
        self.ui.wait_text(MusicPage.TITLE_ONE if count == 1 else MusicPage.TITLE_PREFIX)


class PlayerPage:
    CURRENT_PLAYER = "Current player:"
    TIME_RE = re.compile(r"\b(\d{2}:\d{2})\b")

    def __init__(self, ui):
        self.ui = ui

    def current_player_name(self) -> str | None:
        label = self.ui.read(r"Current player:")
        if not label:
            return None
        rest = label.split("Current player:", 1)[1]
        return rest.split(" / ")[0].strip()

    def has_song_title(self, song: str, row_label: str | None) -> bool:
        # Exclude the Music list row itself and look for the song name shown in the player.
        return any(song in l and l != row_label for l in self.ui.labels())

    def time_tokens(self, row_label: str | None) -> list[str]:
        tokens = []
        for label in self.ui.labels():
            if row_label and label == row_label:
                continue
            tokens.extend(self.TIME_RE.findall(label))
        return tokens

    def progress(self, total: str, row_label: str | None) -> tuple[int | None, bool]:
        """Return (current progress in seconds, whether the total duration is shown)."""
        tokens = self.time_tokens(row_label)
        has_total = total in tokens
        current = [t for t in tokens if t != total]
        return (_secs(current[0]) if current else None), has_total
