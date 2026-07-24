#version 330

in vec2 fragTexCoord;
in vec4 fragColor;

uniform sampler2D texture0;
uniform vec4 blur_rect_uv;    // { x, y, width, height } in UV coordinates
uniform float blur_radius;
uniform vec2 direction;       // (1.0, 0.0) or (0.0, 1.0)

out vec4 finalColor;

void main()
{
    // 指定された矩形内のみブラーを適用
    bool in_rect = fragTexCoord.x >= blur_rect_uv.x &&
                   fragTexCoord.x <= blur_rect_uv.x + blur_rect_uv.z &&
                   fragTexCoord.y >= blur_rect_uv.y &&
                   fragTexCoord.y <= blur_rect_uv.y + blur_rect_uv.w;

    if (in_rect && blur_radius > 0.5)
    {
        vec2 texelSize = 1.0 / vec2(textureSize(texture0, 0));
        vec4 blurred_color = vec4(0.0);

        float sigma = blur_radius / 2.0;
        // 3σ まで取る。±2σ で切ると裾がピーク比 13.5% のまま途切れ、明るい文字や
        // 細い線の周りに輪郭のあるブルーム状の棚が残る。3σ なら 1.1% で見えない。
        int taps = int(ceil(sigma * 3.0));
        float total_weight = 0.0;

        vec2 rect_min = blur_rect_uv.xy;
        vec2 rect_max = blur_rect_uv.xy + blur_rect_uv.zw;

        for (int i = -taps; i <= taps; i++) {
            vec2 offset = direction * texelSize * float(i);
            vec2 sampleCoord = fragTexCoord + offset;

            // 矩形の外は重みごと捨てて正規化し直す（外のピクセル（ボーダーなど）が
            // 滲まないように）。clamp で境界に張り付かせると端の 1 列に半カーネル分の
            // 重みが集中して、縁に帯が出る。
            if (any(lessThan(sampleCoord, rect_min)) || any(greaterThan(sampleCoord, rect_max))) {
                continue;
            }

            float dist = float(i);
            float weight = exp(-(dist * dist) / (2.0 * sigma * sigma));

            blurred_color += texture(texture0, sampleCoord) * weight;
            total_weight += weight;
        }

        finalColor = (blurred_color / total_weight) * fragColor;
    }
    else
    {
        finalColor = texture(texture0, fragTexCoord) * fragColor;
    }
}
