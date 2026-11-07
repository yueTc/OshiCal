FROM python:3.12-slim

WORKDIR /app

# 纯 HTTP 方案，不需要 chromium / 中文字体等重型依赖
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY laplace_scraper_http.py .

ENV TZ=Asia/Shanghai
ENV OUT_DIR=/data

# 默认抓一次增量；配合 cron / k8s CronJob 定时跑
CMD ["sh", "-c", "python laplace_scraper_http.py --out $OUT_DIR --workers 6 --rate 6"]
