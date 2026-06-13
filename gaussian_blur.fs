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

        int radius = int(blur_radius);
        float sigma = float(radius) / 2.0;
        float total_weight = 0.0;

        for (int i = -radius; i <= radius; i++) {
            vec2 offset = direction * texelSize * float(i);
            vec2 sampleCoord = fragTexCoord + offset;

            // blur_rect_uv 内にクランプ（外のピクセル（ボーダーなど）が滲まないように）
            sampleCoord = clamp(sampleCoord, blur_rect_uv.xy, blur_rect_uv.xy + blur_rect_uv.zw);

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
