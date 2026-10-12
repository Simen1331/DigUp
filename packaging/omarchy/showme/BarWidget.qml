import QtQuick
import Quickshell
import Quickshell.Io
import qs.Ui

// ShowMe in the Omarchy bar, as its Waybar module is elsewhere: how far indexing has got while it runs, hidden when
// all is read. `showme status --waybar --follow` prints a line of Waybar JSON each time the daemon's state changes,
// and keeps trying while the daemon is stopped. A click shows the search panel, a middle click pauses or resumes.
BarWidget {
  id: root
  moduleName: "showme"

  property string label: ""
  property string tooltip: "ShowMe"
  property string daemonState: ""

  function update(line) {
    var data
    try {
      data = JSON.parse(line)
    } catch (error) {
      return
    }
    root.label = data.text || ""
    root.tooltip = data.tooltip || "ShowMe"
    root.daemonState = data.class || ""
  }

  Process {
    id: status
    command: ["bash", "-lc", "exec showme status --waybar --follow"]
    running: true
    stdout: SplitParser {
      onRead: function (line) { root.update(line) }
    }
    // Not installed yet, or the shell lost it: try again in a while.
    onExited: restart.start()
  }

  Timer {
    id: restart
    interval: 10000
    onTriggered: status.running = true
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.label
    tooltipText: root.tooltip
    active: root.daemonState === "error"
    dimmed: root.daemonState === "paused" || root.daemonState === "waiting"
    onPressed: function (mouseButton) {
      if (mouseButton === Qt.MiddleButton)
        Quickshell.execDetached(["bash", "-lc", "showme pause --toggle"])
      else if (mouseButton === Qt.LeftButton)
        Quickshell.execDetached(["bash", "-lc", "showme-panel --toggle"])
    }
  }
}
