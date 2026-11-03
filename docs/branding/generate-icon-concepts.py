from PIL import Image, ImageDraw, ImageFont, ImageFilter
import math, os

OUT = os.path.abspath("docs/branding")
ICON_DIR = os.path.join(OUT, "icons")
os.makedirs(ICON_DIR, exist_ok=True)

SIZE = 2048
PREVIEW = 1024
PINK = "#E94F88"
PINK_LIGHT = "#F46CA0"
PINK_DEEP = "#B83263"
CREAM = "#FFF8FB"
PLUM = "#241820"
GREEN = "#16A46B"
WHITE = "#FFFFFF"

FONT_CN_BOLD = r"C:\Windows\Fonts\msyhbd.ttc"
FONT_CN = r"C:\Windows\Fonts\msyh.ttc"
FONT_EN_BOLD = r"C:\Windows\Fonts\segoeuib.ttf"
FONT_EN = r"C:\Windows\Fonts\segoeui.ttf"


def font(path, size):
    return ImageFont.truetype(path, size)


def gradient(size, c1, c2):
    img = Image.new("RGB", (size, size), c1)
    p = img.load()
    a = tuple(int(c1[i:i+2], 16) for i in (1,3,5))
    b = tuple(int(c2[i:i+2], 16) for i in (1,3,5))
    for y in range(size):
        for x in range(size):
            t = (x + y) / (2 * (size - 1))
            p[x,y] = tuple(round(a[i]*(1-t) + b[i]*t) for i in range(3))
    return img


def rounded_mask(size, radius):
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    d.rounded_rectangle((0,0,size-1,size-1), radius=radius, fill=255)
    return m


def base_icon(c1=PINK, c2=PINK_LIGHT):
    g = gradient(SIZE, c1, c2).convert("RGBA")
    g.putalpha(rounded_mask(SIZE, 440))
    return g


def heart(draw, cx, cy, scale, fill):
    pts=[]
    for i in range(360):
        t=math.radians(i)
        x=16*math.sin(t)**3
        y=13*math.cos(t)-5*math.cos(2*t)-2*math.cos(3*t)-math.cos(4*t)
        pts.append((cx+x*scale, cy-y*scale))
    draw.polygon(pts, fill=fill)


def play_triangle(draw, box, fill):
    x0,y0,x1,y1=box
    draw.rounded_polygon if False else None
    draw.polygon([(x0,y0),(x0,y1),(x1,(y0+y1)/2)], fill=fill)


def icon_a():
    img=base_icon()
    d=ImageDraw.Draw(img)
    # calendar binding rings
    d.rounded_rectangle((625,280,745,500), radius=55, fill=CREAM)
    d.rounded_rectangle((1303,280,1423,500), radius=55, fill=CREAM)
    # calendar body
    d.rounded_rectangle((350,390,1698,1660), radius=190, outline=CREAM, width=68)
    d.line((350,700,1698,700), fill=CREAM, width=58)
    # date dots
    for x,y in [(620,860),(830,860),(1040,860),(1250,860)]:
        d.ellipse((x-27,y-27,x+27,y+27), fill=CREAM)
    # play symbol in lower grid cell
    d.rounded_rectangle((560,1030,1488,1500), radius=110, fill=CREAM)
    play_triangle(d, (835,1135,1245,1400), PINK)
    # live notification dot
    d.ellipse((1488,318,1788,618), fill=CREAM)
    d.ellipse((1530,360,1746,576), fill=GREEN)
    return img


def icon_b():
    img=base_icon(PINK_DEEP, PINK)
    d=ImageDraw.Draw(img)
    # bell handle and body
    d.ellipse((900,250,1148,500), outline=CREAM, width=64)
    body=[
        (1024,430),(760,560),(655,850),(620,1190),(500,1370),
        (1548,1370),(1428,1190),(1393,850),(1288,560)
    ]
    d.polygon(body, fill=CREAM)
    d.ellipse((500,1300,1548,1510), fill=CREAM)
    d.rounded_rectangle((500,1420,1548,1530), radius=55, fill=CREAM)
    # clapper play triangle negative space
    play_triangle(d, (845,885,1220,1190), PINK_DEEP)
    # ring arcs
    d.arc((247,660,720,1250), start=130, end=215, fill=CREAM, width=44)
    d.arc((1328,660,1801,1250), start=325, end=50, fill=CREAM, width=44)
    # live dot
    d.ellipse((1510,260,1810,560), fill=CREAM)
    d.ellipse((1552,302,1768,518), fill=GREEN)
    return img


def icon_c():
    img=base_icon(PLUM, "#442435")
    d=ImageDraw.Draw(img)
    # soft glow
    glow=Image.new("RGBA",(SIZE,SIZE),(0,0,0,0))
    gd=ImageDraw.Draw(glow)
    for r,alpha in [(740,20),(620,30),(500,50),(390,60)]:
        gd.ellipse((1024-r,1024-r,1024+r,1024+r), fill=(255,118,168,alpha))
    glow=glow.filter(ImageFilter.GaussianBlur(45))
    img.alpha_composite(glow)
    d=ImageDraw.Draw(img)
    # lantern top + bottom
    d.rounded_rectangle((864,265,1184,365), radius=50, fill=CREAM)
    d.rounded_rectangle((790,1480,1258,1605), radius=62, fill=CREAM)
    d.line((1024,365,1024,440), fill=CREAM, width=38)
    # lantern body
    d.rounded_rectangle((570,410,1478,1510), radius=320, fill=CREAM)
    # inner dark window
    d.rounded_rectangle((725,585,1323,1340), radius=240, fill=PLUM)
    # glowing play-flame
    play_triangle(d, (900,735,1215,1185), PINK_LIGHT)
    # tiny sparkle/live dot
    d.ellipse((1375,430,1545,600), fill=GREEN)
    return img


def icon_d():
    img=base_icon(CREAM, "#FFEAF2")
    d=ImageDraw.Draw(img)
    # curtain columns with wavy inner edge approximated by polygons + circles
    left=[(0,0),(620,0),(560,280),(690,520),(555,790),(705,1050),(545,1320),(680,1600),(0,1600)]
    right=[(2048,0),(1428,0),(1488,280),(1358,520),(1493,790),(1343,1050),(1503,1320),(1368,1600),(2048,1600)]
    d.polygon(left, fill=PINK_DEEP)
    d.polygon(right, fill=PINK_DEEP)
    # stage floor
    d.rounded_rectangle((430,1525,1618,1715), radius=80, fill=PINK)
    # center play
    d.rounded_rectangle((690,575,1358,1370), radius=250, fill=PINK)
    play_triangle(d, (945,725,1215,1215), CREAM)
    # calendar dots
    for x in (820,1024,1228):
        d.ellipse((x-33,365,x+33,431), fill=PINK)
    # live dot
    d.ellipse((1480,250,1780,550), fill=CREAM)
    d.ellipse((1522,292,1738,508), fill=GREEN)
    return img


icons=[icon_a(),icon_b(),icon_c(),icon_d()]
names=["A 推历 / OshiCal","B 播铃 / StreamBell","C 守灯 / Lightkeeper","D 幕历 / StageCal"]
for i,(im,name) in enumerate(zip(icons,names),1):
    im.resize((PREVIEW,PREVIEW), Image.Resampling.LANCZOS).save(os.path.join(ICON_DIR,f"concept-{chr(96+i)}.png"))
    im.resize((512,512), Image.Resampling.LANCZOS).save(os.path.join(ICON_DIR,f"concept-{chr(96+i)}-512.png"))

# concept board
W,H=2048,1100
board=Image.new("RGB",(W,H),"#FFF8FB")
bd=ImageDraw.Draw(board)
# subtle top decoration
for i in range(30):
    bd.ellipse((1650-i*18,55-i*8,1820-i*18,225-i*8), outline=(249,203,219), width=2)
title_f=font(FONT_CN_BOLD,82)
sub_f=font(FONT_CN,32)
label_f=font(FONT_CN_BOLD,40)
desc_f=font(FONT_CN,27)
small_f=font(FONT_EN,23)
bd.text((120,82),"图标方向：日程 + 开播 + 提醒",font=title_f,fill=PLUM)
bd.text((124,190),"围绕 OshiCal 的品牌核心，做四套可落地方向；推荐 A。",font=sub_f,fill="#75636D")

icon_x=[110,590,1070,1550]
icon_y=290
preview_size=390
for i,(im,x) in enumerate(zip(icons,icon_x)):
    # shadow
    shadow=Image.new("RGBA",(preview_size+80,preview_size+80),(0,0,0,0))
    sd=ImageDraw.Draw(shadow)
    sd.rounded_rectangle((40,55,40+preview_size,55+preview_size),radius=88,fill=(67,25,44,32))
    shadow=shadow.filter(ImageFilter.GaussianBlur(18))
    board.paste(shadow,(x-40,icon_y-40),shadow)
    pic=im.resize((preview_size,preview_size),Image.Resampling.LANCZOS)
    board.paste(pic,(x,icon_y),pic)
    bd.text((x,720),names[i],font=label_f,fill=PLUM)

descs=[
    "日历格 + 播放键 + 开播绿点\n最贴合「我推的直播节目单」",
    "铃铛 + 播放三角 + 声波\n功能直接，适合通知型产品",
    "深色底 + 发光灯体 + 播放键\n情绪更强，适合粉丝文化",
    "幕布 + 播放键 + 日历圆点\n舞台感强，适合节目单定位",
]
for i,x in enumerate(icon_x):
    bd.multiline_text((x,790),descs[i],font=desc_f,fill="#75636D",spacing=12)

# recommendation strip
bd.rounded_rectangle((110,930,1938,1032),radius=34,fill="#FFFFFF",outline="#EEDDE5",width=2)
bd.rounded_rectangle((138,956,188,1006),radius=14,fill=PINK)
bd.text((216,950),"推荐 A：日历页是主轮廓，播放键承担「直播」语义，绿点承担「正在开播」语义。16px 时仍可只保留日历 + 播放键。",font=font(FONT_CN,28),fill=PLUM)
bd.text((1910,945),"01",font=font(FONT_EN_BOLD,42),fill="#EEDDE5",anchor="ra")

board.save(os.path.join(OUT,"icon-concepts.png"),quality=95)
print(os.path.join(OUT,"icon-concepts.png"))

