"""TC-10 用到的 Page Object：语聊房 -> More -> Room Tools -> Music -> 播放器。

Hafa 是 Flutter 应用，所有定位只用 content-desc，坐标仅做兜底。
"""
import re


class RoomPage:
    """语聊房主界面。"""

    # 房间底部 More 九宫格图标：没有任何 content-desc，只能用固定坐标（树里排在 "Chat" 之后）
    MORE_ICON_XY = (295, 1527)
    # 点开 More 后面板里最有代表性的文字
    MORE_PANEL_MARK = "Room Tools"

    def __init__(self, ui):
        self.ui = ui

    def open_more(self):
        self.ui.tap_xy(*self.MORE_ICON_XY)
        self.ui.wait_text(self.MORE_PANEL_MARK)
        return MorePanel(self.ui)


class MorePanel:
    """More 面板（Interaction Tools / Room Tools / Others）。"""

    MUSIC = ("Music", None)

    def __init__(self, ui):
        self.ui = ui

    def open_music(self):
        text, xy = self.MUSIC
        self.ui.tap(text, xy=xy, exact=True)
        self.ui.wait_text(MusicPage.ADD_MUSIC[0])
        return MusicPage(self.ui)


class MusicPage:
    """房间音乐列表（个人音乐库）。"""

    ADD_MUSIC = ("Add Music", (360, 1532))
    EDIT = ("Edit", None)
    # 标题形如 "Music(2/50)"
    COUNT_PATTERN = r"^Music\(\d+/\d+\)$"

    def __init__(self, ui):
        self.ui = ui

    def wait_loaded(self):
        self.ui.wait_text(self.ADD_MUSIC[0])

    def song_count(self):
        """读取标题里的歌曲数，读不到返回 None。"""
        label = self.ui.read(self.COUNT_PATTERN)
        if label is None:
            return None
        m = re.match(r"^Music\((\d+)/\d+\)$", label)
        return int(m.group(1)) if m else None

    def has_song(self, name):
        """列表里是否有该歌曲（content-desc 形如 "歌名 / 00:30"）。"""
        return any(lbl == name or lbl.startswith(name + " / ") for lbl in self.ui.labels())

    def song_label(self, name):
        """返回该歌曲的完整 content-desc（含时长），没有返回 None。"""
        for lbl in self.ui.labels():
            if lbl == name or lbl.startswith(name + " / "):
                return lbl
        return None

    def play_song(self, name):
        # 列表顺序每次不同：按歌名定位，必要时上滑
        self.ui.tap(name + " / ", exact=False, scroll=True)
        # 点歌后列表页本身不新增文字，等列表仍在即可（播放状态由后续读数确认）
        self.ui.wait_text(self.ADD_MUSIC[0])


class PlayerPage:
    """播放器界面（含 Next、My Music Library 入口）。"""

    MY_MUSIC_LIBRARY = ("My Music Library", None)
    NEXT = "Next"
    # 播放进度时间，例如 "00:12"
    TIME_PATTERN = r"^\d{1,2}:\d{2}$"

    def __init__(self, ui):
        self.ui = ui

    def read_time(self):
        """读取界面上第一个时间读数，读不到返回 None（播放器的时间不一定暴露在 content-desc 中）。"""
        return self.ui.read(self.TIME_PATTERN)

    def next_label(self):
        """返回包含 "Next" 的那条文字（如 "Next: tst_normal_01"），没有返回 None。"""
        for lbl in self.ui.labels():
            if lbl.startswith(self.NEXT):
                return lbl
        return None

    def open_my_music_library(self):
        text, xy = self.MY_MUSIC_LIBRARY
        self.ui.tap(text, xy=xy, exact=True)
        self.ui.wait_text(MusicPage.ADD_MUSIC[0])
        return MusicPage(self.ui)
