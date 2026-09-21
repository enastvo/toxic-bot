from PIL import Image, ImageFilter
import math
src = Image.open('johny.png').convert('RGB')
W,H = src.size
def radial_mask(w,h, inner, outer):
    mw = 360; mh = int(mw*h/w)
    m = Image.new('L',(mw,mh),0); px = m.load()
    cx,cy = mw/2, mh/2; maxd = math.hypot(cx,cy)
    for y in range(mh):
        for x in range(mw):
            d = math.hypot(x-cx,y-cy)/maxd
            t = max(0.0,min(1.0,(d-inner)/(outer-inner)))
            px[x,y] = int(255*(1-t))
    return m.resize((w,h), Image.BICUBIC).filter(ImageFilter.GaussianBlur(w/60))
def make(darkness, vig, out):
    black = Image.new('RGB',(W,H),(0,0,0))
    img = Image.blend(src, black, darkness)
    mask = radial_mask(W,H, 0.12, 1.08)
    vigimg = Image.composite(img, black, mask)
    img = Image.blend(img, vigimg, vig)
    img.save(out, quality=82, optimize=True)
    import os; print(out, f"{os.path.getsize(out)//1024}KB")
make(0.46, 0.85, 'login-bg.jpg')   # subdued (login)
make(0.70, 0.90, 'app-bg.jpg')     # darker subdued (logged-in)
