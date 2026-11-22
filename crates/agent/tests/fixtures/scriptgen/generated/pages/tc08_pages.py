"""TC-08 Room volume 用到的 Page Object。

Hafa 是 Flutter 应用，只按 content-desc 定位，坐标只做兜底。
Room volume 滑块是自绘控件，没有可定位的文字，只能按坐标拖动或点击。
拖动起点必须落在滑块圆点上，否则手势会落到遮罩（Scrim）上，把面板关掉。
所以拖动前先用点击把圆点移到一个已知位置（锚点），再从锚点开始拖。

修正说明（上一版失败原因）：
房间 ID 不是固定值。conftest 每次运行都会新开一个语聊房，轨迹里是 ID:1081203，
自检时是 ID:1081206。上一版把 "ID:1081203" 写死成等待文案，所以重启后一直等不到。
现在改成：用例开始时从房间标题里读出当前房间 ID，重启后先等标题出现，
再核对是不是同一个房间。
"""

import re


class RoomPage:
    """语聊房主页。"""

    MUSIC = ("Music", None)            # 房间里的音乐入口
    ROOM_ID_PREFIX = "ID:"             # 房间标题形如 "AI Auto Test Room / ID:1081206"
    ROOM_ID_RE = r"ID:\s*(\d+)"

    def __init__(self, ui):
        self.ui = ui

    def room_id(self):
        """从房间标题里读出当前房间 ID（如 "1081206"），读不到返回 None。"""
        for label in self.ui.labels():
            m = re.search(self.ROOM_ID_RE, label)
            if m:
                return m.group(1)
        return None

    def wait_loaded(self, timeout: float = 20):
        # 只等房间标题里固定的 "ID:" 前缀，不写死具体房间号（每次运行都不同）
        self.ui.wait_text(self.ROOM_ID_PREFIX, timeout=timeout)
        # 进房后可能弹出活动弹窗或语音调试面板，它的遮罩会挡住下面的控件
        self.ui.close_popups()
        self.ui.dismiss_debug_overlay()
        self.ui.wait_text(self.MUSIC[0], exact=True)
        return self

    def open_music(self) -> None:
        """点房间的 Music。已在播歌时会直接进播放器，否则进歌曲列表。"""
        text, xy = self.MUSIC
        self.ui.tap(text, xy=xy, exact=True)
        self.ui.wait_any([MusicPage.TITLE_PREFIX, PlayerPage.CURRENT_PLAYER])

    def open_music_list(self) -> "MusicPage":
        text, xy = self.MUSIC
        self.ui.tap(text, xy=xy, exact=True)
        self.ui.wait_text(MusicPage.TITLE_PREFIX)
        return MusicPage(self.ui)


class MusicPage:
    """房间音乐列表（Music(n/50)）。列表顺序每次不同，只按歌名定位。"""

    TITLE_PREFIX = "Music("
    ADD_MUSIC = ("Add Music", None)

    def __init__(self, ui):
        self.ui = ui

    def play_song(self, song: str) -> "PlayerPage":
        # 歌曲行的 content-desc 形如 "tst_normal_01 / 00:30"，用包含匹配
        self.ui.tap(song, exact=False, scroll=True)
        # 点歌后会播放，并弹出音乐播放器
        self.ui.wait_text(PlayerPage.ROOM_VOLUME[0], exact=True)
        return PlayerPage(self.ui)


class PlayerPage:
    """音乐播放器弹层。"""

    ROOM_VOLUME = ("Room volume", None)       # 播放器顶部左侧喇叭图标
    CURRENT_PLAYER = "Current player:"
    TURN_OFF_MUSIC = "Turn off music"

    def __init__(self, ui):
        self.ui = ui

    def wait_loaded(self, timeout: float = 10):
        self.ui.wait_text(self.CURRENT_PLAYER, timeout=timeout)
        return self

    def open_room_volume(self) -> "RoomVolumePanel":
        text, xy = self.ROOM_VOLUME
        self.ui.tap(text, xy=xy, exact=True)
        # 进入 Room volume 后，播放器的内容（Current player:）会消失，面板顶部出现返回用的 Music
        self.ui.wait_gone(self.CURRENT_PLAYER)
        self.ui.wait_text(RoomVolumePanel.BACK_TO_PLAYER[0], exact=True)
        return RoomVolumePanel(self.ui)


class RoomVolumePanel:
    """Room volume 面板：一条横向滑块，右侧显示百分比。"""

    BACK_TO_PLAYER = ("Music", None)          # 面板顶部的 Music，点击后回到播放器
    TITLE = "Room volume"
    TURN_OFF_MUSIC = "Turn off music"
    VALUE_PATTERN = r"^\d+%$"

    # 滑块坐标（来自轨迹，屏宽 720）。轨道大约从 x=38 到 x=677。
    SLIDER_Y = 1431
    MID_X = 360            # 默认 50% 时圆点所在位置
    RIGHT_END_X = 680      # 拖到这里约等于 100%（离右边缘 40px，不会触发侧滑返回）
    ANCHOR_X = 300         # 点击这里，圆点会移到约 41%，作为拖动起点
    ANCHOR_DRAG_X = 303    # 从锚点处起拖
    LEFT_END_X = 40        # 拖到这里约等于 2%（离左边缘 40px）
    LEFT_TAP_X = 45        # 再点一下轨道最左侧，得到 0%
    TARGET_80_X = 550      # 从锚点拖到这里约等于 77%~80%

    def __init__(self, ui):
        self.ui = ui

    # ---------- 读数 ----------
    def value(self):
        """读取当前百分比，读不到返回 None。"""
        s = self.ui.read(self.VALUE_PATTERN)
        return int(s.rstrip("%")) if s else None

    def is_open(self) -> bool:
        return (
            self.ui.has(self.TITLE, exact=True)
            and self.ui.has(self.TURN_OFF_MUSIC, exact=True)
            and self.value() is not None
        )

    # ---------- 操作 ----------
    def _tap_track(self, x: int):
        self.ui.tap_xy(x, self.SLIDER_Y, wait=1.0)

    def _drag_track(self, x1: int, x2: int):
        # 自绘滑块松手后才回写数值，用 drag 自带的等待让数值稳定
        self.ui.drag(x1, self.SLIDER_Y, x2, self.SLIDER_Y, duration=0.8, wait=1.5)

    def drag_to_max(self) -> int:
        """先把圆点放到中间，再从中间拖到最右。"""
        self._tap_track(self.MID_X)
        self._drag_track(self.MID_X, self.RIGHT_END_X)
        return self.value()

    def drag_to_min(self) -> int:
        """先点锚点把圆点移到约 41%，再拖到最左，最后点一下最左端补到 0%。"""
        self._tap_track(self.ANCHOR_X)
        self._drag_track(self.ANCHOR_DRAG_X, self.LEFT_END_X)
        self._tap_track(self.LEFT_TAP_X)
        return self.value()

    def drag_to_about_80(self) -> int:
        """先点锚点把圆点移到约 41%，再拖到约 80% 的位置。"""
        self._tap_track(self.ANCHOR_X)
        self._drag_track(self.ANCHOR_DRAG_X, self.TARGET_80_X)
        return self.value()

    def back_to_player(self) -> PlayerPage:
        text, xy = self.BACK_TO_PLAYER
        self.ui.tap(text, xy=xy, exact=True)
        self.ui.wait_text(PlayerPage.CURRENT_PLAYER)
        return PlayerPage(self.ui)
