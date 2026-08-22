import QtQuick
import QtQuick.Shapes
import qs.Commons

Item {
    id: root

    property color color: Color.foreground
    property real iconSize: Style.font.icon

    implicitWidth: iconSize
    implicitHeight: iconSize
    width: iconSize
    height: iconSize

    Shape {
        anchors.fill: parent
        antialiasing: true
        layer.enabled: true
        layer.samples: 4

        ShapePath {
            fillColor: "transparent"
            strokeColor: root.color
            strokeWidth: Math.max(1.25, root.width * 0.105)
            capStyle: ShapePath.RoundCap
            joinStyle: ShapePath.RoundJoin
            startX: root.width * 0.08
            startY: root.height * 0.08

            PathLine {
                x: root.width * 0.5
                y: root.height * 0.92
            }

            PathLine {
                x: root.width * 0.92
                y: root.height * 0.08
            }

        }

        ShapePath {
            fillColor: "transparent"
            strokeColor: root.color
            strokeWidth: Math.max(1, root.width * 0.07)
            capStyle: ShapePath.RoundCap
            startX: root.width * 0.5
            startY: root.height * 0.04

            PathLine {
                x: root.width * 0.5
                y: root.height * 0.96
            }

        }

        ShapePath {
            fillColor: "transparent"
            strokeColor: root.color
            strokeWidth: Math.max(1, root.width * 0.07)
            capStyle: ShapePath.RoundCap
            startX: root.width * 0.04
            startY: root.height * 0.5

            PathLine {
                x: root.width * 0.96
                y: root.height * 0.5
            }

        }

    }

}
